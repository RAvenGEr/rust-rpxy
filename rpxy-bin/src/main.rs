#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod config;
mod constants;
mod error;
mod log;

#[cfg(feature = "acme")]
use crate::config::build_acme_manager;
use crate::{
  config::{build_cert_manager, build_settings, ConfigToml, ConfigTomlReloader, Opts, Parser},
  constants::CONFIG_WATCH_DELAY_SECS,
  error::*,
  log::*,
};
use async_trait::async_trait;
use hot_reload::{ReloaderReceiver, ReloaderService};
use rpxy_lib::{entrypoint, RpxyOptions, RpxyOptionsBuilder};
use shellflip::lifecycle::*;
use shellflip::{RestartConfig, ShutdownCoordinator, ShutdownHandle, ShutdownSignal};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::{pin, select};
use tokio_util::sync::CancellationToken;

struct RestartData {
  restart_generation: u32,
}

fn main() {
  init_logger();
  let parsed_opts = Opts::parse();

  let config_toml = match ConfigToml::new(&parsed_opts.config_file_path) {
    Ok(conf) => conf,
    Err(e) => {
      error!("Invalid toml file: {e}");
      std::process::exit(1);
    }
  };
  let mut runtime_builder = tokio::runtime::Builder::new_multi_thread();
  runtime_builder.enable_all();
  runtime_builder.thread_name("rpxy");
  let runtime = runtime_builder.build().unwrap();

  runtime.block_on(async {
    const RESTART_SOCKET: &str = "/run/rpxy/restart.sock";

    let mut app_data = RestartData { restart_generation: 0 };

    if let Some(mut handover_pipe) = receive_from_old_process() {
      app_data.restart_generation = handover_pipe.read_u32().await.expect("Handover failure") + 1;
    }

    let restart_generation = app_data.restart_generation;

    // Configure the essential requirements for implementing graceful restart.
    let restart_conf = RestartConfig {
      enabled: true,
      coordination_socket_path: RESTART_SOCKET.into(),
      lifecycle_handler: Box::new(app_data),
      ..Default::default()
    };

    if parsed_opts.restart {
      let res = restart_conf.request_restart().await;
      match res {
        Ok(id) => {
          info!("Restart succeeded, child pid is {}", id);
          std::process::exit(0);
        }
        Err(e) => {
          error!("Restart failed: {}", e);
          std::process::exit(1);
        }
      }
    }

    // Start the restart thread and get a task that will complete when a restart completes.
    let restart_task = restart_conf.try_into_restart_task().unwrap();
    // (need to pin this because of the loop below!)
    pin!(restart_task);

    // Restart incompatible with watch for now
    if !parsed_opts.watch {
      let cancel_token = tokio_util::sync::CancellationToken::new();
      select! {
        res = rpxy_service_without_watcher(&config_toml, runtime.handle().clone(), cancel_token.clone()) => {
          if let Err(e) = res {
            error!("rpxy service existed: {e}");
            std::process::exit(1);
          }
        }
        res = &mut restart_task => {
            match res {
                Ok(_) => {
                      info!("Restart successful, waiting for tasks to complete");
                }
                Err(e) => {
                      error!("Restart task failed: {}", e);
                }
            }
            // Wait for all clients to complete.
            cancel_token.cancel();
            info!("Exiting...");
            std::process::exit(0);
        }
      }
    } else {
      let (config_service, config_rx) = ReloaderService::<ConfigTomlReloader, ConfigToml, String>::new(
        &parsed_opts.config_file_path,
        CONFIG_WATCH_DELAY_SECS,
        false,
      )
      .await
      .unwrap();

      tokio::select! {
        config_res = config_service.start() => {
          if let Err(e) = config_res {
            error!("config reloader service exited: {e}");
            std::process::exit(1);
          }
        }
        rpxy_res = rpxy_service_with_watcher(config_rx, runtime.handle().clone()) => {
          if let Err(e) = rpxy_res {
            error!("rpxy service existed: {e}");
            std::process::exit(1);
          }
        }
      }
      std::process::exit(0);
    }
  });
}

/// rpxy service definition
struct RpxyService {
  runtime_handle: tokio::runtime::Handle,
  proxy_conf: rpxy_lib::ProxyConfig,
  app_conf: rpxy_lib::AppConfigList,
  cert_service: Option<Arc<ReloaderService<rpxy_certs::CryptoReloader, rpxy_certs::ServerCryptoBase>>>,
  cert_rx: Option<ReloaderReceiver<rpxy_certs::ServerCryptoBase>>,
  #[cfg(feature = "acme")]
  acme_manager: Option<rpxy_acme::AcmeManager>,
}

impl RpxyService {
  async fn new(config_toml: &ConfigToml, runtime_handle: tokio::runtime::Handle) -> Result<Self, anyhow::Error> {
    let (proxy_conf, app_conf) = build_settings(config_toml).map_err(|e| anyhow!("Invalid configuration: {e}"))?;

    let (cert_service, cert_rx) = build_cert_manager(config_toml)
      .await
      .map_err(|e| anyhow!("Invalid cert configuration: {e}"))?
      .map(|(s, r)| (Some(Arc::new(s)), Some(r)))
      .unwrap_or((None, None));

    Ok(RpxyService {
      runtime_handle: runtime_handle.clone(),
      proxy_conf,
      app_conf,
      cert_service,
      cert_rx,
      #[cfg(feature = "acme")]
      acme_manager: build_acme_manager(config_toml, runtime_handle.clone()).await?,
    })
  }

  async fn start(&self, cancel_token: CancellationToken) -> Result<(), anyhow::Error> {
    let RpxyService {
      runtime_handle,
      proxy_conf,
      app_conf,
      cert_service: _,
      cert_rx,
      #[cfg(feature = "acme")]
      acme_manager,
    } = self;

    #[cfg(feature = "acme")]
    {
      let (acme_join_handles, server_config_acme_challenge) = acme_manager
        .as_ref()
        .map(|m| m.spawn_manager_tasks(cancel_token.child_token()))
        .unwrap_or((vec![], Default::default()));
      let rpxy_opts = RpxyOptionsBuilder::default()
        .proxy_config(proxy_conf.clone())
        .app_config_list(app_conf.clone())
        .cert_rx(cert_rx.clone())
        .runtime_handle(runtime_handle.clone())
        .server_configs_acme_challenge(Arc::new(server_config_acme_challenge))
        .build()?;
      self
        .start_inner(rpxy_opts, cancel_token, acme_join_handles)
        .await
        .map_err(|e| anyhow!(e))
    }

    #[cfg(not(feature = "acme"))]
    {
      let rpxy_opts = RpxyOptionsBuilder::default()
        .proxy_config(proxy_conf.clone())
        .app_config_list(app_conf.clone())
        .cert_rx(cert_rx.clone())
        .runtime_handle(runtime_handle.clone())
        .build()?;
      self.start_inner(rpxy_opts, cancel_token).await.map_err(|e| anyhow!(e))
    }
  }

  /// Wrapper of entry point for rpxy service with certificate management service
  async fn start_inner(
    &self,
    rpxy_opts: RpxyOptions,
    cancel_token: CancellationToken,
    #[cfg(feature = "acme")] acme_task_handles: Vec<tokio::task::JoinHandle<()>>,
  ) -> Result<(), anyhow::Error> {
    let cancel_token = cancel_token.clone();
    let runtime_handle = rpxy_opts.runtime_handle.clone();

    // spawn rpxy entrypoint, where cancellation token is possibly contained inside the service
    let cancel_token_clone = cancel_token.clone();
    let child_cancel_token = cancel_token.child_token();
    let rpxy_handle = runtime_handle.spawn(async move {
      if let Err(e) = entrypoint(&rpxy_opts, child_cancel_token).await {
        error!("rpxy entrypoint exited on error: {e}");
        cancel_token_clone.cancel();
        return Err(anyhow!(e));
      }
      Ok(())
    });

    if self.cert_service.is_none() {
      return rpxy_handle.await?;
    }

    // spawn certificate reloader service, where cert service does not have cancellation token inside the service
    let cert_service = self.cert_service.as_ref().unwrap().clone();
    let cancel_token_clone = cancel_token.clone();
    let child_cancel_token = cancel_token.child_token();
    let cert_handle = runtime_handle.spawn(async move {
      tokio::select! {
        cert_res = cert_service.start() => {
          if let Err(ref e) = cert_res {
            error!("cert reloader service exited on error: {e}");
          }
          cancel_token_clone.cancel();
          cert_res.map_err(|e| anyhow!(e))
        }
        _ = child_cancel_token.cancelled() => {
          debug!("cert reloader service terminated");
          Ok(())
        }
      }
    });

    #[cfg(not(feature = "acme"))]
    {
      let (rpxy_res, cert_res) = tokio::join!(rpxy_handle, cert_handle);
      let (rpxy_res, cert_res) = (rpxy_res?, cert_res?);
      match (rpxy_res, cert_res) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(e), _) => Err(e),
        (_, Err(e)) => Err(e),
      }
    }

    #[cfg(feature = "acme")]
    {
      if acme_task_handles.is_empty() {
        let (rpxy_res, cert_res) = tokio::join!(rpxy_handle, cert_handle);
        let (rpxy_res, cert_res) = (rpxy_res?, cert_res?);
        return match (rpxy_res, cert_res) {
          (Ok(()), Ok(())) => Ok(()),
          (Err(e), _) => Err(e),
          (_, Err(e)) => Err(e),
        };
      }

      // spawn acme manager tasks, where cancellation token is possibly contained inside the service
      let select_all = futures_util::future::select_all(acme_task_handles);
      let cancel_token_clone = cancel_token.clone();
      let acme_handle = runtime_handle.spawn(async move {
        let (acme_res, _, _) = select_all.await;
        if let Err(ref e) = acme_res {
          error!("acme manager exited on error: {e}");
        }
        cancel_token_clone.cancel();
        acme_res.map_err(|e| anyhow!(e))
      });
      let (rpxy_res, cert_res, acme_res) = tokio::join!(rpxy_handle, cert_handle, acme_handle);
      let (rpxy_res, cert_res, acme_res) = (rpxy_res?, cert_res?, acme_res?);
      match (rpxy_res, cert_res, acme_res) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (Err(e), _, _) => Err(e),
        (_, Err(e), _) => Err(e),
        (_, _, Err(e)) => Err(e),
      }
    }
  }
}

#[async_trait]
impl LifecycleHandler for RestartData {
  async fn send_to_new_process(&mut self, mut write_pipe: PipeWriter) -> std::io::Result<()> {
    if self.restart_generation > 4 {
      log::info!("Four restarts is more than anybody needs, surely?");
      return Err(std::io::Error::new(
        std::io::ErrorKind::Other,
        "The operation completed successfully",
      ));
    }
    write_pipe.write_u32(self.restart_generation).await?;
    Ok(())
  }
}

async fn rpxy_service_without_watcher(
  config_toml: &ConfigToml,
  runtime_handle: tokio::runtime::Handle,
  cancel_token: CancellationToken,
) -> Result<(), anyhow::Error> {
  info!("Start rpxy service");
  let service = RpxyService::new(config_toml, runtime_handle).await?;
  service.start(cancel_token).await
}

async fn rpxy_service_with_watcher(
  mut config_rx: ReloaderReceiver<ConfigToml, String>,
  runtime_handle: tokio::runtime::Handle,
) -> Result<(), anyhow::Error> {
  info!("Start rpxy service with dynamic config reloader");
  // Initial loading
  config_rx.changed().await?;
  let config_toml = config_rx
    .borrow()
    .clone()
    .ok_or(anyhow!("Something wrong in config reloader receiver"))?;
  let mut service = RpxyService::new(&config_toml, runtime_handle.clone()).await?;

  // Continuous monitoring
  loop {
    // Notifier for proxy service termination
    let cancel_token = tokio_util::sync::CancellationToken::new();

    tokio::select! {
      /* ---------- */
      rpxy_res = service.start(cancel_token.clone()) => {
        if let Err(ref e) = rpxy_res {
          error!("rpxy service exited on error: {e}");
        } else {
          error!("rpxy service exited");
        }
        return rpxy_res.map_err(|e| anyhow!(e));
      }
      /* ---------- */
      _ = config_rx.changed() => {
        let Some(new_config_toml) = config_rx.borrow().clone() else {
          error!("Something wrong in config reloader receiver");
          return Err(anyhow!("Something wrong in config reloader receiver"));
        };
        match RpxyService::new(&new_config_toml, runtime_handle.clone()).await {
          Ok(new_service) => {
            info!("Configuration updated.");
            service = new_service;
          },
          Err(e) => {
            error!("rpxy failed to be ready. Configuration does not updated: {e}");
          }
        };
        info!("Terminate all spawned services and force to re-bind TCP/UDP sockets");
        cancel_token.cancel();
      }
    }
  }
}

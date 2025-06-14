use crate::constants::{ACCESS_LOG_FILE, SYSTEM_LOG_FILE};
use rpxy_lib::log_event_names;
use std::str::FromStr;
use tracing_subscriber::{fmt, prelude::*};

#[allow(unused)]
pub use tracing::{debug, error, info, warn};

/// Initialize the logger with the RUST_LOG environment variable.
pub fn init_logger(log_dir_path: Option<&str>) {
  let level_string = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
  let level = tracing::Level::from_str(level_string.as_str()).unwrap_or(tracing::Level::INFO);

  match log_dir_path {
    None => {
      // log to stdout
      init_stdio_logger(level);
    }
    Some(log_dir_path) => {
      // log to files
      println!("Activate logging to files: {log_dir_path}");
      init_file_logger(level, log_dir_path);
    }
  }
}

/// file logging TODO:
fn init_file_logger(level: tracing::Level, log_dir_path: &str) {
  let log_dir_path = std::path::PathBuf::from(log_dir_path);
  // create the directory if it does not exist
  if !log_dir_path.exists() {
    println!("Directory does not exist, creating: {}", log_dir_path.display());
    std::fs::create_dir_all(&log_dir_path).expect("Failed to create log directory");
  }
  let access_log_path = log_dir_path.join(ACCESS_LOG_FILE);
  let system_log_path = log_dir_path.join(SYSTEM_LOG_FILE);
  println!("Access log: {}", access_log_path.display());
  println!("System and error log: {}", system_log_path.display());

  let reg = tracing_subscriber::registry();

  let access_log_appender = tracing_appender::rolling::daily(&log_dir_path, ACCESS_LOG_FILE);
  let (access_non_blocking, _guard) = tracing_appender::non_blocking(access_log_appender);
  let system_log_appender = tracing_appender::rolling::daily(log_dir_path, SYSTEM_LOG_FILE);
  let (system_non_blocking, _guard) = tracing_appender::non_blocking(system_log_appender);

  let access_log_base = fmt::layer()
    .with_line_number(false)
    .with_thread_ids(false)
    .with_thread_names(false)
    .with_target(false)
    .with_level(false)
    .compact()
    .with_ansi(false);
  let reg = reg.with(access_log_base.with_writer(access_non_blocking).with_filter(AccessLogFilter));

  let system_log_base = fmt::layer()
    .with_line_number(false)
    .with_thread_ids(false)
    .with_thread_names(false)
    .with_target(false)
    .with_level(true) // with level for system log
    .compact()
    .with_ansi(false);
  let reg = reg.with(
    system_log_base
      .with_writer(system_non_blocking)
      .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
        (metadata
          .target()
          .starts_with(env!("CARGO_PKG_NAME").replace('-', "_").as_str())
          && metadata.name() != log_event_names::ACCESS_LOG
          && metadata.level() <= &level)
          || metadata.level() <= &tracing::Level::WARN.min(level)
      })),
  );

  reg.init();
}

/// stdio logging
fn init_stdio_logger(level: tracing::Level) {
  // This limits the logger to emits only this crate with any level above RUST_LOG, for included crates it will emit only ERROR (in prod)/INFO (in dev) or above level.
  let stdio_layer = fmt::layer().with_level(true).with_thread_ids(false);
  if level <= tracing::Level::INFO {
    // in normal deployment environment
    let stdio_layer = stdio_layer
      .with_target(false)
      .compact()
      .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
        (metadata
          .target()
          .starts_with(env!("CARGO_PKG_NAME").replace('-', "_").as_str())
          && metadata.level() <= &level)
          || metadata.level() <= &tracing::Level::WARN.min(level)
      }));
    tracing_subscriber::registry().with(stdio_layer).init();
  } else {
    // debugging
    let stdio_layer = stdio_layer
      .with_line_number(true)
      .with_target(true)
      .with_thread_names(true)
      .with_target(true)
      .compact()
      .with_filter(tracing_subscriber::filter::filter_fn(move |metadata| {
        (metadata
          .target()
          .starts_with(env!("CARGO_PKG_NAME").replace('-', "_").as_str())
          && metadata.level() <= &level)
          || metadata.level() <= &tracing::Level::INFO.min(level)
      }));
    tracing_subscriber::registry().with(stdio_layer).init();
  };
}

/// Access log filter
struct AccessLogFilter;
impl<S> tracing_subscriber::layer::Filter<S> for AccessLogFilter {
  fn enabled(&self, metadata: &tracing::Metadata<'_>, _: &tracing_subscriber::layer::Context<'_, S>) -> bool {
    metadata
      .target()
      .starts_with(env!("CARGO_PKG_NAME").replace('-', "_").as_str())
      && metadata.name().contains(log_event_names::ACCESS_LOG)
      && metadata.level() <= &tracing::Level::INFO
  }
}

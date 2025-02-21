// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::{
  helpers::{app_paths::tauri_dir, config::Config as TauriConfig},
  interface::{AppInterface, AppSettings, DevProcess, Interface, Options as InterfaceOptions},
};
use anyhow::{bail, Result};
use heck::ToSnekCase;
use jsonrpsee::core::client::{Client, ClientBuilder, ClientT};
use jsonrpsee::server::{RpcModule, ServerBuilder, ServerHandle};
use jsonrpsee_client_transport::ws::WsTransportClientBuilder;
use jsonrpsee_core::rpc_params;
use serde::{Deserialize, Serialize};

use cargo_mobile2::{
  config::app::{App, Raw as RawAppConfig},
  env::Error as EnvError,
  opts::{NoiseLevel, Profile},
  ChildHandle,
};
use std::{
  collections::HashMap,
  env::{set_var, temp_dir},
  ffi::OsString,
  fmt::Write,
  fs::{read_to_string, write},
  net::SocketAddr,
  path::PathBuf,
  process::{exit, ExitStatus},
  sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
  },
};
use tokio::runtime::Runtime;

#[cfg(not(windows))]
use cargo_mobile2::env::Env;
#[cfg(windows)]
use cargo_mobile2::os::Env;

pub mod android;
mod init;
#[cfg(target_os = "macos")]
pub mod ios;

const MIN_DEVICE_MATCH_SCORE: isize = 0;

#[derive(Clone)]
pub struct DevChild {
  child: Arc<ChildHandle>,
  manually_killed_process: Arc<AtomicBool>,
}

impl DevChild {
  fn new(handle: ChildHandle) -> Self {
    Self {
      child: Arc::new(handle),
      manually_killed_process: Default::default(),
    }
  }
}

impl DevProcess for DevChild {
  fn kill(&self) -> std::io::Result<()> {
    self.manually_killed_process.store(true, Ordering::Relaxed);
    match self.child.kill() {
      Ok(_) => Ok(()),
      Err(e) => {
        self.manually_killed_process.store(false, Ordering::Relaxed);
        Err(e)
      }
    }
  }

  fn try_wait(&self) -> std::io::Result<Option<ExitStatus>> {
    self.child.try_wait().map(|res| res.map(|o| o.status))
  }

  fn wait(&self) -> std::io::Result<ExitStatus> {
    self.child.wait().map(|o| o.status)
  }

  fn manually_killed_process(&self) -> bool {
    self.manually_killed_process.load(Ordering::Relaxed)
  }
}

#[derive(PartialEq, Eq, Copy, Clone)]
pub enum Target {
  Android,
  #[cfg(target_os = "macos")]
  Ios,
}

impl Target {
  fn ide_name(&self) -> &'static str {
    match self {
      Self::Android => "Android Studio",
      #[cfg(target_os = "macos")]
      Self::Ios => "Xcode",
    }
  }

  fn command_name(&self) -> &'static str {
    match self {
      Self::Android => "android",
      #[cfg(target_os = "macos")]
      Self::Ios => "ios",
    }
  }

  fn ide_build_script_name(&self) -> &'static str {
    match self {
      Self::Android => "android-studio-script",
      #[cfg(target_os = "macos")]
      Self::Ios => "xcode-script",
    }
  }

  fn platform_target(&self) -> tauri_utils::platform::Target {
    match self {
      Self::Android => tauri_utils::platform::Target::Android,
      #[cfg(target_os = "macos")]
      Self::Ios => tauri_utils::platform::Target::Ios,
    }
  }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CliOptions {
  pub features: Option<Vec<String>>,
  pub args: Vec<String>,
  pub noise_level: NoiseLevel,
  pub vars: HashMap<String, OsString>,
}

impl Default for CliOptions {
  fn default() -> Self {
    Self {
      features: None,
      args: vec!["--lib".into()],
      noise_level: Default::default(),
      vars: Default::default(),
    }
  }
}

fn env_vars() -> HashMap<String, OsString> {
  let mut vars = HashMap::new();
  vars.insert("RUST_LOG_STYLE".into(), "always".into());
  for (k, v) in std::env::vars_os() {
    let k = k.to_string_lossy();
    if (k.starts_with("TAURI")
      && k != "TAURI_SIGNING_PRIVATE_KEY"
      && k != "TAURI_SIGNING_PRIVATE_KEY_PASSWORD")
      || k.starts_with("WRY")
      || k.starts_with("CARGO_")
      || k == "TMPDIR"
      || k == "PATH"
    {
      vars.insert(k.into_owned(), v);
    }
  }
  vars
}

fn env() -> Result<Env, EnvError> {
  let env = Env::new()?.explicit_env_vars(env_vars());
  Ok(env)
}

pub struct OptionsHandle(#[allow(unused)] Runtime, #[allow(unused)] ServerHandle);

/// Writes CLI options to be used later on the Xcode and Android Studio build commands
pub fn write_options(identifier: &str, mut options: CliOptions) -> crate::Result<OptionsHandle> {
  options.vars.extend(env_vars());

  let runtime = Runtime::new().unwrap();
  let r: anyhow::Result<(ServerHandle, SocketAddr)> = runtime.block_on(async move {
    let server = ServerBuilder::default().build("127.0.0.1:0").await?;
    let addr = server.local_addr()?;

    let mut module = RpcModule::new(());
    module.register_method("options", move |_, _| Some(options.clone()))?;

    let handle = server.start(module);

    Ok((handle, addr))
  });
  let (handle, addr) = r?;

  write(
    temp_dir().join(format!("{identifier}-server-addr")),
    addr.to_string(),
  )?;

  Ok(OptionsHandle(runtime, handle))
}

fn read_options(identifier: &str) -> CliOptions {
  let runtime = tokio::runtime::Runtime::new().unwrap();
  let options = runtime
    .block_on(async move {
      let addr_path = temp_dir().join(format!("{identifier}-server-addr"));
      let (tx, rx) = WsTransportClientBuilder::default()
        .build(
          format!(
            "ws://{}",
            read_to_string(&addr_path).unwrap_or_else(|e| panic!(
              "failed to read missing addr file {}: {e}",
              addr_path.display()
            ))
          )
          .parse()
          .unwrap(),
        )
        .await?;
      let client: Client = ClientBuilder::default().build_with_tokio(tx, rx);
      let options: CliOptions = client.request("options", rpc_params![]).await?;
      Ok::<CliOptions, anyhow::Error>(options)
    })
    .expect("failed to read CLI options");

  for (k, v) in &options.vars {
    set_var(k, v);
  }
  options
}

pub fn get_app(config: &TauriConfig, interface: &AppInterface) -> App {
  let identifier = config
    .identifier
    .rsplit('.')
    .collect::<Vec<&str>>()
    .join(".");

  if identifier.is_empty() {
    log::error!("Bundle identifier set in `tauri.conf.json > identifier` cannot be empty");
    exit(1);
  }

  let app_name = interface
    .app_settings()
    .app_name()
    .unwrap_or_else(|| "app".into());
  let lib_name = interface
    .app_settings()
    .lib_name()
    .unwrap_or_else(|| app_name.to_snek_case());

  let raw = RawAppConfig {
    name: app_name,
    lib_name: Some(lib_name),
    stylized_name: config.product_name.clone(),
    identifier,
    asset_dir: None,
    template_pack: None,
  };

  let app_settings = interface.app_settings();
  App::from_raw(tauri_dir(), raw)
    .unwrap()
    .with_target_dir_resolver(move |target, profile| {
      let bin_path = app_settings
        .app_binary_path(&InterfaceOptions {
          debug: matches!(profile, Profile::Debug),
          target: Some(target.into()),
          ..Default::default()
        })
        .expect("failed to resolve target directory");
      bin_path.parent().unwrap().to_path_buf()
    })
}

fn ensure_init(project_dir: PathBuf, target: Target) -> Result<()> {
  if !project_dir.exists() {
    bail!(
      "{} project directory {} doesn't exist. Please run `tauri {} init` and try again.",
      target.ide_name(),
      project_dir.display(),
      target.command_name(),
    )
  }
  Ok(())
}

fn log_finished(outputs: Vec<PathBuf>, kind: &str) {
  if !outputs.is_empty() {
    let mut printable_paths = String::new();
    for path in &outputs {
      writeln!(printable_paths, "        {}", path.display()).unwrap();
    }

    log::info!(action = "Finished"; "{} {}{} at:\n{}", outputs.len(), kind, if outputs.len() == 1 { "" } else { "s" }, printable_paths);
  }
}

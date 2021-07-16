// Copyright 2019-2021 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::helpers::{
  app_paths::{app_dir, tauri_dir},
  config::{get as get_config, reload as reload_config},
  manifest::{get_workspace_members, rewrite_manifest},
  Logger,
};

use anyhow::Context;
use notify::{watcher, DebouncedEvent, RecursiveMode, Watcher};
use once_cell::sync::OnceCell;
use shared_child::SharedChild;

use std::{
  env::set_current_dir,
  ffi::OsStr,
  process::{exit, Child, Command},
  sync::{
    mpsc::{channel, Receiver},
    Arc, Mutex,
  },
  time::Duration,
};

static BEFORE_DEV: OnceCell<Mutex<Child>> = OnceCell::new();

fn kill_before_dev_process() {
  if let Some(child) = BEFORE_DEV.get() {
    let mut child = child.lock().unwrap();
    #[cfg(windows)]
    let _ = Command::new("powershell")
      .arg("-NoProfile")
      .arg("-Command")
      .arg(format!("function Kill-Tree {{ Param([int]$ppid); Get-CimInstance Win32_Process | Where-Object {{ $_.ParentProcessId -eq $ppid }} | ForEach-Object {{ Kill-Tree $_.ProcessId }}; Stop-Process -Id $ppid }}; Kill-Tree {}", child.id()))
      .status();
    #[cfg(not(windows))]
    let _ = Command::new("pkill")
      .args(&["-TERM", "-P"])
      .arg(child.id().to_string())
      .status();
    let _ = child.kill();
  }
}

#[derive(Default)]
pub struct Dev {
  runner: Option<String>,
  target: Option<String>,
  features: Option<Vec<String>>,
  exit_on_panic: bool,
  config: Option<String>,
  args: Vec<String>,
  release_mode: bool,
}

impl Dev {
  pub fn new() -> Self {
    Default::default()
  }

  pub fn runner(mut self, runner: String) -> Self {
    self.runner.replace(runner);
    self
  }

  pub fn target(mut self, target: String) -> Self {
    self.target.replace(target);
    self
  }

  pub fn features(mut self, features: Vec<String>) -> Self {
    self.features.replace(features);
    self
  }

  pub fn config(mut self, config: String) -> Self {
    self.config.replace(config);
    self
  }

  pub fn exit_on_panic(mut self, exit_on_panic: bool) -> Self {
    self.exit_on_panic = exit_on_panic;
    self
  }

  pub fn args(mut self, args: Vec<String>) -> Self {
    self.args = args;
    self
  }

  pub fn release_mode(mut self, release_mode: bool) -> Self {
    self.release_mode = release_mode;
    self
  }

  pub fn run(self) -> crate::Result<()> {
    let logger = Logger::new("tauri:dev");
    let tauri_path = tauri_dir();
    set_current_dir(&tauri_path).with_context(|| "failed to change current working directory")?;
    let merge_config = self.config.clone();
    let config = get_config(merge_config.as_deref())?;
    let mut process: Arc<SharedChild>;

    let (settings, out_dir) = {
      let config_guard = config.lock().unwrap();
      let config_ = config_guard.as_ref().unwrap();
      let app_settings = crate::interface::rust::AppSettings::new(config_)?;
      let out_dir = app_settings
        .get_out_dir(self.target.clone(), true)
        .with_context(|| "failed to get project out directory")?;
      let settings = crate::interface::get_bundler_settings(
        app_settings,
        self.target.clone(),
        &Default::default(),
        config_,
        &out_dir,
        false,
        None,
      )
      .with_context(|| "failed to build bundler settings")?;
      (settings, out_dir)
    };
    settings.copy_resources(&out_dir)?;
    settings.copy_binaries(&out_dir)?;

    if let Some(before_dev) = &config
      .lock()
      .unwrap()
      .as_ref()
      .unwrap()
      .build
      .before_dev_command
    {
      if !before_dev.is_empty() {
        logger.log(format!("Running `{}`", before_dev));
        #[cfg(target_os = "windows")]
        let child = Command::new("cmd")
          .arg("/C")
          .arg(before_dev)
          .current_dir(app_dir())
          .spawn()
          .with_context(|| format!("failed to run `{}` with `cmd /C`", before_dev))?;
        #[cfg(not(target_os = "windows"))]
        let child = Command::new("sh")
          .arg("-c")
          .arg(before_dev)
          .current_dir(app_dir())
          .spawn()
          .with_context(|| format!("failed to run `{}` with `sh -c`", before_dev))?;
        BEFORE_DEV.set(Mutex::new(child)).unwrap();
      }
    }

    let runner_from_config = config
      .lock()
      .unwrap()
      .as_ref()
      .unwrap()
      .build
      .runner
      .clone();
    let runner = self
      .runner
      .clone()
      .or(runner_from_config)
      .unwrap_or_else(|| "cargo".to_string());

    {
      let (tx, rx) = channel();
      let mut watcher = watcher(tx, Duration::from_secs(1)).unwrap();
      watcher.watch(tauri_path.join("Cargo.toml"), RecursiveMode::Recursive)?;
      rewrite_manifest(config.clone())?;
      loop {
        if let Ok(DebouncedEvent::NoticeWrite(_)) = rx.recv() {
          break;
        }
      }
    }

    let mut cargo_features = config
      .lock()
      .unwrap()
      .as_ref()
      .unwrap()
      .build
      .features
      .clone()
      .unwrap_or_default();
    if let Some(features) = &self.features {
      cargo_features.extend(features.clone());
    }

    let (child_wait_tx, child_wait_rx) = channel();
    let child_wait_rx = Arc::new(Mutex::new(child_wait_rx));

    process = self.start_app(&runner, &cargo_features, child_wait_rx.clone());

    let (tx, rx) = channel();

    let mut watcher = watcher(tx, Duration::from_secs(1)).unwrap();
    watcher.watch(tauri_path.join("src"), RecursiveMode::Recursive)?;
    watcher.watch(tauri_path.join("Cargo.toml"), RecursiveMode::Recursive)?;
    watcher.watch(tauri_path.join("tauri.conf.json"), RecursiveMode::Recursive)?;

    for member in get_workspace_members()? {
      let workspace_path = tauri_path.join(member);
      watcher.watch(workspace_path.join("src"), RecursiveMode::Recursive)?;
      watcher.watch(workspace_path.join("Cargo.toml"), RecursiveMode::Recursive)?;
    }

    loop {
      if let Ok(event) = rx.recv() {
        let event_path = match event {
          DebouncedEvent::Create(path) => Some(path),
          DebouncedEvent::Remove(path) => Some(path),
          DebouncedEvent::Rename(_, dest) => Some(dest),
          DebouncedEvent::Write(path) => Some(path),
          _ => None,
        };

        if let Some(event_path) = event_path {
          if event_path.file_name() == Some(OsStr::new("tauri.conf.json")) {
            reload_config(merge_config.as_deref())?;
            rewrite_manifest(config.clone())?;
          } else {
            // When tauri.conf.json is changed, rewrite_manifest will be called
            // which will trigger the watcher again
            // So the app should only be started when a file other than tauri.conf.json is changed
            let _ = child_wait_tx.send(());
            process
              .kill()
              .with_context(|| "failed to kill app process")?;
            // wait for the process to exit
            loop {
              if let Ok(Some(_)) = process.try_wait() {
                break;
              }
            }
            process = self.start_app(&runner, &cargo_features, child_wait_rx.clone());
          }
        }
      }
    }
  }

  fn start_app(
    &self,
    runner: &str,
    features: &[String],
    child_wait_rx: Arc<Mutex<Receiver<()>>>,
  ) -> Arc<SharedChild> {
    let mut command = Command::new(runner);
    command.args(&["run", "--no-default-features"]);

    if self.release_mode {
      command.args(&["--release"]);
    }

    if let Some(target) = &self.target {
      command.args(&["--target", target]);
    }

    if !features.is_empty() {
      command.args(&["--features", &features.join(",")]);
    }

    if !self.args.is_empty() {
      command.arg("--").args(&self.args);
    }

    let child =
      SharedChild::spawn(&mut command).unwrap_or_else(|_| panic!("failed to run {}", runner));
    let child_arc = Arc::new(child);

    let child_clone = child_arc.clone();
    let exit_on_panic = self.exit_on_panic;
    std::thread::spawn(move || {
      let status = child_clone.wait().expect("failed to wait on child");
      if exit_on_panic {
        // we exit if the status is a success code (app closed) or code is 101 (compilation error)
        // if the process wasn't killed by the file watcher
        if (status.success() || status.code() == Some(101))
          // `child_wait_rx` indicates that the process was killed by the file watcher
          && child_wait_rx
          .lock()
          .expect("failed to get child_wait_rx lock")
          .try_recv()
          .is_err()
        {
          kill_before_dev_process();
          exit(0);
        }
      } else if status.success() {
        // if we're no exiting on panic, we only exit if the status is a success code (app closed)
        kill_before_dev_process();
        exit(0);
      }
    });

    child_arc
  }
}

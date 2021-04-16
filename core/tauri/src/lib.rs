// Copyright 2019-2021 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

//! Tauri is a framework for building tiny, blazing fast binaries for all major desktop platforms.
//! Developers can integrate any front-end framework that compiles to HTML, JS and CSS for building their user interface.
//! The backend of the application is a rust-sourced binary with an API that the front-end can interact with.
//!
//! The user interface in Tauri apps currently leverages Cocoa/WebKit on macOS, gtk-webkit2 on Linux and MSHTML (IE10/11) or Webkit via Edge on Windows.
//! Tauri uses (and contributes to) the MIT licensed project that you can find at [webview](https://github.com/webview/webview).
#![warn(missing_docs, rust_2018_idioms)]

pub(crate) use crate::api::private::async_runtime;
/// The Tauri error enum.
pub use error::Error;
pub use tauri_macros::{command, generate_handler};

pub mod api;
/// The Tauri API endpoints.
mod endpoints;
mod error;
mod event;
mod hooks;
pub mod plugin;
pub mod runtime;
/// The Tauri-specific settings for your runtime e.g. notification permission status.
pub mod settings;
#[cfg(feature = "updater")]
pub mod updater;

/// `Result<T, ::tauri::Error>`
pub type Result<T> = std::result::Result<T, Error>;

/// A task to run on the main thread.
pub type SyncTask = Box<dyn FnOnce() + Send>;

use crate::api::assets::Assets;
use crate::api::config::Config;
use crate::event::{Event, EventHandler};
use crate::runtime::tag::Tag;
use crate::runtime::window::PendingWindow;
use crate::runtime::{Dispatch, Runtime};
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;

// Export types likely to be used by the application.
pub use {
  api::config::WindowUrl,
  hooks::InvokeMessage,
  runtime::app::{App, Builder},
  runtime::webview::Attributes,
  runtime::window::export::Window,
};

/// Reads the config file at compile time and generates a [`Context`] based on its content.
///
/// The default config file path is a `tauri.conf.json` file inside the Cargo manifest directory of
/// the crate being built.
///
/// # Custom Config Path
///
/// You may pass a string literal to this macro to specify a custom path for the Tauri config file.
/// If the path is relative, it will be search for relative to the Cargo manifest of the compiling
/// crate.
///
/// # Note
///
/// This macro should not be called if you are using [`tauri-build`] to generate the context from
/// inside your build script as it will just cause excess computations that will be discarded. Use
/// either the [`tauri-build] method or this macro - not both.
///
/// [`tauri-build`]: https://docs.rs/tauri-build
pub use tauri_macros::generate_context;

/// Include a [`Context`] that was generated by [`tauri-build`] inside your build script.
///
/// You should either use [`tauri-build`] and this macro to include the compile time generated code,
/// or [`generate_context!`]. Do not use both at the same time, as they generate the same code and
/// will cause excess computations that will be discarded.
///
/// [`tauri-build`]: https://docs.rs/tauri-build
#[macro_export]
macro_rules! tauri_build_context {
  () => {
    include!(concat!(env!("OUT_DIR"), "/tauri-build-context.rs"))
  };
}

/// A icon definition.
pub enum Icon {
  /// Icon from file path.
  File(PathBuf),
  /// Icon from raw bytes.
  Raw(Vec<u8>),
}

/// User supplied data required inside of a Tauri application.
pub struct Context<A: Assets> {
  /// The config the application was prepared with.
  pub config: Config,

  /// The assets to be served directly by Tauri.
  pub assets: A,

  /// The default window icon Tauri should use when creating windows.
  pub default_window_icon: Option<Vec<u8>>,

  /// Package information.
  pub package_info: crate::api::PackageInfo,
}

/// Types associated with the running Tauri application.
pub trait Params: sealed::ParamsBase {
  /// The event type used to create and listen to events.
  type Event: Tag;

  /// The type used to determine the name of windows.
  type Label: Tag;

  /// Assets that Tauri should serve from itself.
  type Assets: Assets;

  /// The underlying webview runtime used by the Tauri application.
  type Runtime: Runtime;
}

/// Manages a running application.
///
/// TODO: expand these docs
pub trait Manager<M: Params>: sealed::ManagerBase<M> {
  /// The [`Config`] the manager was created with.
  fn config(&self) -> &Config {
    self.manager().config()
  }

  /// Emits a event to all windows.
  fn emit_all<S: Serialize + Clone>(&self, event: M::Event, payload: Option<S>) -> Result<()> {
    self.manager().emit_filter(event, payload, |_| true)
  }

  /// Emits an event to a window with the specified label.
  fn emit_to<S: Serialize + Clone>(
    &self,
    label: &M::Label,
    event: M::Event,
    payload: Option<S>,
  ) -> Result<()> {
    self
      .manager()
      .emit_filter(event, payload, |w| w.label() == label)
  }

  /// Creates a new [`Window`] on the [`Runtime`] and attaches it to the [`Manager`].
  fn create_window(&mut self, pending: PendingWindow<M>) -> Result<Window<M>> {
    use sealed::RuntimeOrDispatch::*;

    let labels = self.manager().labels().into_iter().collect::<Vec<_>>();
    let pending = self.manager().prepare_window(pending, &labels)?;
    match self.runtime() {
      Runtime(runtime) => runtime.create_window(pending),
      Dispatch(mut dispatcher) => dispatcher.create_window(pending),
    }
    .map(|window| self.manager().attach_window(window))
  }

  /// Listen to a global event.
  fn listen_global<F>(&self, event: M::Event, handler: F) -> EventHandler
  where
    F: Fn(Event) + Send + 'static,
  {
    self.manager().listen(event, None, handler)
  }

  /// Listen to a global event only once.
  fn once_global<F>(&self, event: M::Event, handler: F) -> EventHandler
  where
    F: Fn(Event) + Send + 'static,
  {
    self.manager().once(event, None, handler)
  }

  /// Trigger a global event.
  fn trigger_global(&self, event: M::Event, data: Option<String>) {
    self.manager().trigger(event, None, data)
  }

  /// Remove an event listener.
  fn unlisten(&self, handler_id: EventHandler) {
    self.manager().unlisten(handler_id)
  }

  /// Fetch a single window from the manager.
  fn get_window(&self, label: &M::Label) -> Option<Window<M>> {
    self.manager().get_window(label)
  }

  /// Fetch all managed windows.
  fn windows(&self) -> HashMap<M::Label, Window<M>> {
    self.manager().windows()
  }
}

/// Prevent implementation details from leaking out of the [`Manager`] and [`Params`] traits.
pub(crate) mod sealed {
  use super::Params;
  use crate::runtime::{manager::WindowManager, Runtime};

  /// No downstream implementations of [`Params`].
  pub trait ParamsBase: 'static {}

  /// A running [`Runtime`] or a dispatcher to it.
  pub enum RuntimeOrDispatch<'r, P: Params> {
    /// Mutable reference to the running [`Runtime`].
    Runtime(&'r mut P::Runtime),

    /// A dispatcher to the running [`Runtime`].
    Dispatch(<P::Runtime as Runtime>::Dispatcher),
  }

  /// Managed handle to the application runtime.
  pub trait ManagerBase<P: Params> {
    /// The manager behind the [`Managed`] item.
    fn manager(&self) -> &WindowManager<P>;

    /// The runtime or runtime dispatcher of the [`Managed`] item.
    fn runtime(&mut self) -> RuntimeOrDispatch<'_, P>;
  }
}

#[cfg(test)]
mod test {
  use proptest::prelude::*;

  proptest! {
    #![proptest_config(ProptestConfig::with_cases(10000))]
    #[test]
    // check to see if spawn executes a function.
    fn check_spawn_task(task in "[a-z]+") {
      // create dummy task function
      let dummy_task = async move {
        format!("{}-run-dummy-task", task);
      };
      // call spawn
      crate::async_runtime::spawn(dummy_task);
    }
  }
}
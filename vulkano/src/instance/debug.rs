// Copyright (c) 2016 The vulkano developers
// Licensed under the Apache License, Version 2.0
// <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT
// license <LICENSE-MIT or http://opensource.org/licenses/MIT>,
// at your option. All files in the project carrying such
// notice may not be copied, modified, or distributed except
// according to those terms.

//! Debug callback called by intermediate layers or by the driver.
//!
//! When working on an application, it is recommended to register a debug callback. For example if
//! you enable the validation layers provided by the official Vulkan SDK, they will warn you about
//! invalid API usages or performance problems by calling this callback. The callback can also
//! be called by the driver or by whatever intermediate layer is activated.
//!
//! Note that the vulkano library can also emit messages to warn you about performance issues.
//! TODO: ^ that's not the case yet, need to choose whether we keep this idea
//!
//! # Example
//!
//! ```
//! # use vulkano::instance::Instance;
//! # use std::sync::Arc;
//! # let instance: Arc<Instance> = return;
//! use vulkano::instance::debug::DebugCallback;
//!
//! let _callback = DebugCallback::errors_and_warnings(&instance, |msg| {
//!     println!("Debug callback: {:?}", msg.description);
//! }).ok();
//! ```
//!
//! The type of `msg` in the callback is [`Message`](struct.Message.html).
//!
//! Note that you must keep the `_callback` object alive for as long as you want your callback to
//! be callable. If you don't store the return value of `DebugCallback`'s constructor in a
//! variable, it will be immediately destroyed and your callback will not work.
//!

use std::error;
use std::ffi::CStr;
use std::fmt;
use std::mem;
use std::os::raw::{c_char, c_void};
use std::panic;
use std::ptr;
use std::sync::Arc;

use instance::Instance;

use check_errors;
use vk;
use Error;
use VulkanObject;

/// Registration of a callback called by validation layers.
///
/// The callback can be called as long as this object is alive.
#[must_use = "The DebugCallback object must be kept alive for as long as you want your callback \
              to be called"]
pub struct DebugCallback {
  instance:              Arc<Instance>,
  debug_report_callback: vk::DebugReportCallbackEXT,
  user_callback:         Box<Box<Fn(&Message)>>,
}

impl DebugCallback {
  /// Initializes a debug callback.
  ///
  /// Panics generated by calling `user_callback` are ignored.
  pub fn new<F>(
    instance: &Arc<Instance>,
    messages: MessageTypes,
    user_callback: F,
  ) -> Result<DebugCallback, DebugCallbackCreationError>
  where
    F: Fn(&Message) + 'static + Send + panic::RefUnwindSafe,
  {
    if !instance.loaded_extensions().ext_debug_report {
      return Err(DebugCallbackCreationError::MissingExtension);
    }

    // Note that we need to double-box the callback, because a `*const Fn()` is a fat pointer
    // that can't be cast to a `*const c_void`.
    let user_callback = Box::new(Box::new(user_callback) as Box<_>);

    extern "system" fn callback(
      ty: vk::DebugReportFlagsEXT,
      _: vk::DebugReportObjectTypeEXT,
      _: u64,
      _: usize,
      _: i32,
      layer_prefix: *const c_char,
      description: *const c_char,
      user_data: *mut c_void,
    ) -> u32 {
      unsafe {
        let user_callback = user_data as *mut Box<Fn()> as *const _;
        let user_callback: &Box<Fn(&Message)> = &*user_callback;

        let layer_prefix = CStr::from_ptr(layer_prefix)
          .to_str()
          .expect("debug callback message not utf-8");
        let description = CStr::from_ptr(description)
          .to_str()
          .expect("debug callback message not utf-8");

        let message = Message {
          ty:           MessageTypes {
            information:         (ty & vk::DEBUG_REPORT_INFORMATION_BIT_EXT) != 0,
            warning:             (ty & vk::DEBUG_REPORT_WARNING_BIT_EXT) != 0,
            performance_warning: (ty & vk::DEBUG_REPORT_PERFORMANCE_WARNING_BIT_EXT) != 0,
            error:               (ty & vk::DEBUG_REPORT_ERROR_BIT_EXT) != 0,
            debug:               (ty & vk::DEBUG_REPORT_DEBUG_BIT_EXT) != 0,
          },
          layer_prefix: layer_prefix,
          description:  description,
        };

        // Since we box the closure, the type system doesn't detect that the `UnwindSafe`
        // bound is enforced. Therefore we enforce it manually.
        let _ = panic::catch_unwind(panic::AssertUnwindSafe(move || {
          user_callback(&message);
        }));

        vk::FALSE
      }
    }

    let flags = {
      let mut flags = 0;
      if messages.information {
        flags |= vk::DEBUG_REPORT_INFORMATION_BIT_EXT;
      }
      if messages.warning {
        flags |= vk::DEBUG_REPORT_WARNING_BIT_EXT;
      }
      if messages.performance_warning {
        flags |= vk::DEBUG_REPORT_PERFORMANCE_WARNING_BIT_EXT;
      }
      if messages.error {
        flags |= vk::DEBUG_REPORT_ERROR_BIT_EXT;
      }
      if messages.debug {
        flags |= vk::DEBUG_REPORT_DEBUG_BIT_EXT;
      }
      flags
    };

    let infos = vk::DebugReportCallbackCreateInfoEXT {
      sType:       vk::STRUCTURE_TYPE_DEBUG_REPORT_CALLBACK_CREATE_INFO_EXT,
      pNext:       ptr::null(),
      flags:       flags,
      pfnCallback: callback,
      pUserData:   &*user_callback as &Box<_> as *const Box<_> as *const c_void as *mut _,
    };

    let vk = instance.pointers();

    let debug_report_callback = unsafe {
      let mut output = mem::uninitialized();
      check_errors(vk.CreateDebugReportCallbackEXT(
        instance.internal_object(),
        &infos,
        ptr::null(),
        &mut output,
      ))?;
      output
    };

    Ok(DebugCallback {
      instance:              instance.clone(),
      debug_report_callback: debug_report_callback,
      user_callback:         user_callback,
    })
  }

  /// Initializes a debug callback with errors and warnings.
  ///
  /// Shortcut for `new(instance, MessageTypes::errors_and_warnings(), user_callback)`.
  #[inline]
  pub fn errors_and_warnings<F>(
    instance: &Arc<Instance>,
    user_callback: F,
  ) -> Result<DebugCallback, DebugCallbackCreationError>
  where
    F: Fn(&Message) + Send + 'static + panic::RefUnwindSafe,
  {
    DebugCallback::new(instance, MessageTypes::errors_and_warnings(), user_callback)
  }
}

impl Drop for DebugCallback {
  #[inline]
  fn drop(&mut self) {
    unsafe {
      let vk = self.instance.pointers();
      vk.DestroyDebugReportCallbackEXT(
        self.instance.internal_object(),
        self.debug_report_callback,
        ptr::null(),
      );
    }
  }
}

/// A message received by the callback.
pub struct Message<'a> {
  /// Type of message.
  pub ty: MessageTypes,
  /// Prefix of the layer that reported this message.
  pub layer_prefix: &'a str,
  /// Description of the message.
  pub description: &'a str,
}

/// Type of message.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct MessageTypes {
  /// An error that may cause undefined results, including an application crash.
  pub error: bool,
  /// An unexpected use.
  pub warning: bool,
  /// A potential non-optimal use.
  pub performance_warning: bool,
  /// An informational message that may be handy when debugging an application.
  pub information: bool,
  /// Diagnostic information from the loader and layers.
  pub debug: bool,
}

impl MessageTypes {
  /// Builds a `MessageTypes` with all fields set to `false` expect `error`.
  #[inline]
  pub fn errors() -> MessageTypes {
    MessageTypes {
      error: true,
      ..MessageTypes::none()
    }
  }

  /// Builds a `MessageTypes` with all fields set to `false` expect `error`, `warning`
  /// and `performance_warning`.
  #[inline]
  pub fn errors_and_warnings() -> MessageTypes {
    MessageTypes {
      error: true,
      warning: true,
      performance_warning: true,
      ..MessageTypes::none()
    }
  }

  /// Builds a `MessageTypes` with all fields set to `false`.
  #[inline]
  pub fn none() -> MessageTypes {
    MessageTypes {
      error:               false,
      warning:             false,
      performance_warning: false,
      information:         false,
      debug:               false,
    }
  }
}

/// Error that can happen when creating a debug callback.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum DebugCallbackCreationError {
  /// The `EXT_debug_report` extension was not enabled.
  MissingExtension,
}

impl error::Error for DebugCallbackCreationError {
  #[inline]
  fn description(&self) -> &str {
    match *self {
      DebugCallbackCreationError::MissingExtension => "the `EXT_debug_report` extension was not enabled",
    }
  }
}

impl fmt::Display for DebugCallbackCreationError {
  #[inline]
  fn fmt(&self, fmt: &mut fmt::Formatter) -> Result<(), fmt::Error> {
    write!(fmt, "{}", error::Error::description(self))
  }
}

impl From<Error> for DebugCallbackCreationError {
  #[inline]
  fn from(err: Error) -> DebugCallbackCreationError {
    panic!("unexpected error: {:?}", err)
  }
}

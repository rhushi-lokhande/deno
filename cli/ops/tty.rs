// Copyright 2018-2020 the Deno authors. All rights reserved. MIT license.
use super::io::std_file_resource;
use super::io::{StreamResource, StreamResourceHolder};
use crate::state::State;
use deno_core::CoreIsolate;
use deno_core::ErrBox;
use deno_core::ResourceTable;
use deno_core::ZeroCopyBuf;
#[cfg(unix)]
use nix::sys::termios;
use serde_derive::{Deserialize, Serialize};
use serde_json::Value;
use std::rc::Rc;

#[cfg(windows)]
use winapi::shared::minwindef::DWORD;
#[cfg(windows)]
use winapi::um::wincon;
#[cfg(windows)]
const RAW_MODE_MASK: DWORD = wincon::ENABLE_LINE_INPUT
  | wincon::ENABLE_ECHO_INPUT
  | wincon::ENABLE_PROCESSED_INPUT;
#[cfg(windows)]
fn get_windows_handle(
  f: &std::fs::File,
) -> Result<std::os::windows::io::RawHandle, ErrBox> {
  use std::os::windows::io::AsRawHandle;
  use winapi::um::handleapi;

  let handle = f.as_raw_handle();
  if handle == handleapi::INVALID_HANDLE_VALUE {
    return Err(ErrBox::last_os_error());
  } else if handle.is_null() {
    return Err(ErrBox::new("ReferenceError", "null handle"));
  }
  Ok(handle)
}

pub fn init(i: &mut CoreIsolate, s: &Rc<State>) {
  let t = &CoreIsolate::state(i).borrow().resource_table.clone();

  i.register_op("op_set_raw", s.stateful_json_op_sync(t, op_set_raw));
  i.register_op("op_isatty", s.stateful_json_op_sync(t, op_isatty));
  i.register_op(
    "op_console_size",
    s.stateful_json_op_sync(t, op_console_size),
  );
}

#[derive(Deserialize)]
struct SetRawArgs {
  rid: u32,
  mode: bool,
}

fn op_set_raw(
  state: &State,
  resource_table: &mut ResourceTable,
  args: Value,
  _zero_copy: &mut [ZeroCopyBuf],
) -> Result<Value, ErrBox> {
  state.check_unstable("Deno.setRaw");
  let args: SetRawArgs = serde_json::from_value(args)?;
  let rid = args.rid;
  let is_raw = args.mode;

  // From https://github.com/kkawakam/rustyline/blob/master/src/tty/windows.rs
  // and https://github.com/kkawakam/rustyline/blob/master/src/tty/unix.rs
  // and https://github.com/crossterm-rs/crossterm/blob/e35d4d2c1cc4c919e36d242e014af75f6127ab50/src/terminal/sys/windows.rs
  // Copyright (c) 2015 Katsu Kawakami & Rustyline authors. MIT license.
  // Copyright (c) 2019 Timon. MIT license.
  #[cfg(windows)]
  {
    use std::os::windows::io::AsRawHandle;
    use winapi::shared::minwindef::FALSE;
    use winapi::um::{consoleapi, handleapi};

    let resource_holder = resource_table.get_mut::<StreamResourceHolder>(rid);
    if resource_holder.is_none() {
      return Err(ErrBox::bad_resource_id());
    }
    let resource_holder = resource_holder.unwrap();

    // For now, only stdin.
    let handle = match &mut resource_holder.resource {
      StreamResource::Stdin(..) => std::io::stdin().as_raw_handle(),
      StreamResource::FsFile(ref mut option_file_metadata) => {
        if let Some((tokio_file, metadata)) = option_file_metadata.take() {
          match tokio_file.try_into_std() {
            Ok(std_file) => {
              let raw_handle = std_file.as_raw_handle();
              // Turn the std_file handle back into a tokio file, put it back
              // in the resource table.
              let tokio_file = tokio::fs::File::from_std(std_file);
              resource_holder.resource =
                StreamResource::FsFile(Some((tokio_file, metadata)));
              // return the result.
              raw_handle
            }
            Err(tokio_file) => {
              // This function will return an error containing the file if
              // some operation is in-flight.
              resource_holder.resource =
                StreamResource::FsFile(Some((tokio_file, metadata)));
              return Err(ErrBox::resource_unavailable());
            }
          }
        } else {
          return Err(ErrBox::resource_unavailable());
        }
      }
      _ => {
        return Err(ErrBox::bad_resource_id());
      }
    };

    if handle == handleapi::INVALID_HANDLE_VALUE {
      return Err(ErrBox::last_os_error());
    } else if handle.is_null() {
      return Err(ErrBox::new("ReferenceError", "null handle"));
    }
    let mut original_mode: DWORD = 0;
    if unsafe { consoleapi::GetConsoleMode(handle, &mut original_mode) }
      == FALSE
    {
      return Err(ErrBox::last_os_error());
    }
    let new_mode = if is_raw {
      original_mode & !RAW_MODE_MASK
    } else {
      original_mode | RAW_MODE_MASK
    };
    if unsafe { consoleapi::SetConsoleMode(handle, new_mode) } == FALSE {
      return Err(ErrBox::last_os_error());
    }

    Ok(json!({}))
  }
  #[cfg(unix)]
  {
    use std::os::unix::io::AsRawFd;

    let resource_holder = resource_table.get_mut::<StreamResourceHolder>(rid);
    if resource_holder.is_none() {
      return Err(ErrBox::bad_resource_id());
    }

    if is_raw {
      let (raw_fd, maybe_tty_mode) =
        match &mut resource_holder.unwrap().resource {
          StreamResource::Stdin(_, ref mut metadata) => {
            (std::io::stdin().as_raw_fd(), &mut metadata.mode)
          }
          StreamResource::FsFile(Some((f, ref mut metadata))) => {
            (f.as_raw_fd(), &mut metadata.tty.mode)
          }
          StreamResource::FsFile(None) => {
            return Err(ErrBox::resource_unavailable())
          }
          _ => {
            return Err(ErrBox::not_supported());
          }
        };

      if maybe_tty_mode.is_some() {
        // Already raw. Skip.
        return Ok(json!({}));
      }

      let original_mode = termios::tcgetattr(raw_fd)?;
      let mut raw = original_mode.clone();
      // Save original mode.
      maybe_tty_mode.replace(original_mode);

      raw.input_flags &= !(termios::InputFlags::BRKINT
        | termios::InputFlags::ICRNL
        | termios::InputFlags::INPCK
        | termios::InputFlags::ISTRIP
        | termios::InputFlags::IXON);

      raw.control_flags |= termios::ControlFlags::CS8;

      raw.local_flags &= !(termios::LocalFlags::ECHO
        | termios::LocalFlags::ICANON
        | termios::LocalFlags::IEXTEN
        | termios::LocalFlags::ISIG);
      raw.control_chars[termios::SpecialCharacterIndices::VMIN as usize] = 1;
      raw.control_chars[termios::SpecialCharacterIndices::VTIME as usize] = 0;
      termios::tcsetattr(raw_fd, termios::SetArg::TCSADRAIN, &raw)?;
      Ok(json!({}))
    } else {
      // Try restore saved mode.
      let (raw_fd, maybe_tty_mode) =
        match &mut resource_holder.unwrap().resource {
          StreamResource::Stdin(_, ref mut metadata) => {
            (std::io::stdin().as_raw_fd(), &mut metadata.mode)
          }
          StreamResource::FsFile(Some((f, ref mut metadata))) => {
            (f.as_raw_fd(), &mut metadata.tty.mode)
          }
          StreamResource::FsFile(None) => {
            return Err(ErrBox::resource_unavailable());
          }
          _ => {
            return Err(ErrBox::bad_resource_id());
          }
        };

      if let Some(mode) = maybe_tty_mode.take() {
        termios::tcsetattr(raw_fd, termios::SetArg::TCSADRAIN, &mode)?;
      }

      Ok(json!({}))
    }
  }
}

#[derive(Deserialize)]
struct IsattyArgs {
  rid: u32,
}

fn op_isatty(
  _state: &State,
  resource_table: &mut ResourceTable,
  args: Value,
  _zero_copy: &mut [ZeroCopyBuf],
) -> Result<Value, ErrBox> {
  let args: IsattyArgs = serde_json::from_value(args)?;
  let rid = args.rid;

  let isatty: bool =
    std_file_resource(resource_table, rid as u32, move |r| match r {
      Ok(std_file) => {
        #[cfg(windows)]
        {
          use winapi::um::consoleapi;

          let handle = get_windows_handle(&std_file)?;
          let mut test_mode: DWORD = 0;
          // If I cannot get mode out of console, it is not a console.
          Ok(unsafe { consoleapi::GetConsoleMode(handle, &mut test_mode) != 0 })
        }
        #[cfg(unix)]
        {
          use std::os::unix::io::AsRawFd;
          let raw_fd = std_file.as_raw_fd();
          Ok(unsafe { libc::isatty(raw_fd as libc::c_int) == 1 })
        }
      }
      Err(StreamResource::FsFile(_)) => unreachable!(),
      Err(StreamResource::Stdin(..)) => Ok(atty::is(atty::Stream::Stdin)),
      _ => Ok(false),
    })?;
  Ok(json!(isatty))
}

#[derive(Deserialize)]
struct ConsoleSizeArgs {
  rid: u32,
}

#[derive(Serialize)]
struct ConsoleSize {
  columns: u32,
  rows: u32,
}

fn op_console_size(
  state: &State,
  resource_table: &mut ResourceTable,
  args: Value,
  _zero_copy: &mut [ZeroCopyBuf],
) -> Result<Value, ErrBox> {
  state.check_unstable("Deno.consoleSize");
  let args: ConsoleSizeArgs = serde_json::from_value(args)?;
  let rid = args.rid;

  let size = std_file_resource(resource_table, rid as u32, move |r| match r {
    Ok(std_file) => {
      #[cfg(windows)]
      {
        use std::os::windows::io::AsRawHandle;
        let handle = std_file.as_raw_handle();

        unsafe {
          let mut bufinfo: winapi::um::wincon::CONSOLE_SCREEN_BUFFER_INFO =
            std::mem::zeroed();

          if winapi::um::wincon::GetConsoleScreenBufferInfo(
            handle,
            &mut bufinfo,
          ) == 0
          {
            return Err(ErrBox::last_os_error());
          }

          Ok(ConsoleSize {
            columns: bufinfo.dwSize.X as u32,
            rows: bufinfo.dwSize.Y as u32,
          })
        }
      }

      #[cfg(unix)]
      {
        use std::os::unix::io::AsRawFd;

        let fd = std_file.as_raw_fd();
        unsafe {
          let mut size: libc::winsize = std::mem::zeroed();
          if libc::ioctl(fd, libc::TIOCGWINSZ, &mut size as *mut _) != 0 {
            return Err(ErrBox::last_os_error());
          }

          // TODO (caspervonb) return a tuple instead
          Ok(ConsoleSize {
            columns: size.ws_col as u32,
            rows: size.ws_row as u32,
          })
        }
      }
    }
    Err(_) => Err(ErrBox::bad_resource_id()),
  })?;

  Ok(json!(size))
}

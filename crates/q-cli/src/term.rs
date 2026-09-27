//! Terminal size for commands that fit their output to the screen.

/// Columns and rows of the terminal on stdout, when stdout is a terminal.
pub fn size() -> Option<(usize, usize)> {
    imp::size().filter(|(cols, rows)| *cols > 0 && *rows > 0)
}

#[cfg(unix)]
mod imp {
    pub fn size() -> Option<(usize, usize)> {
        // SAFETY: TIOCGWINSZ fills a plain winsize struct and reads nothing
        // else; a failure returns -1 and leaves the zeroed struct untouched.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
        if ok != 0 {
            return None;
        }
        Some((usize::from(ws.ws_col), usize::from(ws.ws_row)))
    }
}

#[cfg(windows)]
mod imp {
    use windows_sys::Win32::System::Console::{
        GetConsoleScreenBufferInfo, GetStdHandle, CONSOLE_SCREEN_BUFFER_INFO, STD_OUTPUT_HANDLE,
    };

    pub fn size() -> Option<(usize, usize)> {
        // SAFETY: the handle comes from the process's own std output and the
        // struct is only written by the call when it succeeds.
        unsafe {
            let handle = GetStdHandle(STD_OUTPUT_HANDLE);
            let mut info: CONSOLE_SCREEN_BUFFER_INFO = std::mem::zeroed();
            if GetConsoleScreenBufferInfo(handle, &mut info) == 0 {
                return None;
            }
            let cols = i32::from(info.srWindow.Right) - i32::from(info.srWindow.Left) + 1;
            let rows = i32::from(info.srWindow.Bottom) - i32::from(info.srWindow.Top) + 1;
            Some((cols.max(0) as usize, rows.max(0) as usize))
        }
    }
}

#[cfg(not(any(unix, windows)))]
mod imp {
    pub fn size() -> Option<(usize, usize)> {
        None
    }
}

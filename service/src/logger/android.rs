use super::{common, LogColor, LogFormat};
use ndk_sys::{__android_log_print, android_LogPriority as LogPriority};
use os_pipe::PipeWriter;
use ouisync_tracing_fmt::Formatter;
use paranoid_android::{AndroidLogMakeWriter, Buffer};
use std::{
    ffi::{CStr, CString},
    io::{self, BufRead, BufReader, Stderr, Stdout},
    os::{
        fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd},
        raw::c_int,
    },
    path::Path,
    sync::Mutex,
    thread,
};
use tracing_subscriber::{
    fmt::{self, time::SystemTime},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

pub(super) struct Inner {
    _stdout: Redirect<Stdout, PipeWriter>,
    _stderr: Redirect<Stderr, PipeWriter>,
}

impl Inner {
    pub fn new(
        path: Option<&Path>,
        tag: String,
        _format: LogFormat,
        _color: LogColor,
    ) -> io::Result<Self> {
        let android_log_layer = fmt::layer()
            .event_format(Formatter::<()>::default()) // android log adds its own timestamp
            .with_ansi(false)
            .with_writer(AndroidLogMakeWriter::with_buffer(tag.clone(), Buffer::Main));

        let file_layer = path.map(|path| {
            fmt::layer()
                .event_format(Formatter::<SystemTime>::default())
                .with_ansi(true)
                .with_writer(Mutex::new(common::create_file_writer(path)))
        });

        let tag = CString::new(tag)?;

        tracing_subscriber::registry()
            .with(common::create_log_filter())
            .with(android_log_layer)
            .with(file_layer)
            .try_init()
            // `Err` here just means the logger is already initialized, it's OK to ignore it.
            .unwrap_or(());

        Ok(Self {
            _stdout: redirect(io::stdout(), LogPriority::ANDROID_LOG_DEBUG, tag.clone())?,
            _stderr: redirect(io::stderr(), LogPriority::ANDROID_LOG_ERROR, tag)?,
        })
    }
}

fn redirect<S: AsFd>(
    stream: S,
    priority: LogPriority,
    tag: CString,
) -> io::Result<Redirect<S, PipeWriter>> {
    let (reader, writer) = os_pipe::pipe()?;
    let redirect = Redirect::new(stream, writer)?;

    thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut line = String::new();

        loop {
            match reader.read_line(&mut line) {
                Ok(n) if n > 0 => {
                    // Remove the trailing newline
                    if line.ends_with('\n') {
                        line.pop();
                    }

                    line = print(priority, &tag, line);
                    line.clear();
                }
                Ok(_) => break, // EOF
                Err(error) => {
                    print(LogPriority::ANDROID_LOG_ERROR, &tag, error.to_string());
                    break;
                }
            }
        }
    });

    Ok(redirect)
}

// Prints `message` to the android log using zero allocations. Returns the original message.
fn print(priority: LogPriority, tag: &CStr, message: String) -> String {
    match CString::new(message) {
        Ok(message) => {
            print_cstr(priority, tag, &message);

            // `unwrap` is ok because the `CString` was created from a valid `String`.
            message.into_string().unwrap()
        }
        Err(error) => {
            // message contains internal nul bytes - escape them.

            // `unwrap` is ok because the vector was obtained from a valid `String`.
            let message = String::from_utf8(error.into_vec()).unwrap();
            let escaped = message.replace('\0', "\\0");
            // `unwrap` is ok because we replaced all the internal nul bytes.
            let escaped = CString::new(escaped).unwrap();
            print_cstr(priority, tag, &escaped);

            message
        }
    }
}

fn print_cstr(priority: LogPriority, tag: &CStr, message: &CStr) {
    // SAFETY: both pointers point to valid c-style strings.
    unsafe {
        __android_log_print(priority.0 as c_int, tag.as_ptr(), message.as_ptr());
    }
}

/// Redirect stdout / stderr
struct Redirect<S, D>
where
    S: AsFd,
    D: AsFd,
{
    src: S,
    src_old: OwnedFd,
    _dst: D,
}

impl<S, D> Redirect<S, D>
where
    S: AsFd,
    D: AsFd,
{
    pub fn new(src: S, dst: D) -> io::Result<Self> {
        // Remember the old fd so we can point it to where it pointed before when we are done.
        let src_old = src.as_fd().try_clone_to_owned()?;

        dup2(dst.as_fd(), src.as_fd())?;

        Ok(Self {
            src,
            src_old,
            _dst: dst,
        })
    }
}

impl<S, D> Drop for Redirect<S, D>
where
    S: AsFd,
    D: AsFd,
{
    fn drop(&mut self) {
        if let Err(error) = dup2(self.src_old.as_fd(), self.src.as_fd()) {
            tracing::error!(
                ?error,
                "Failed to point the redirected file descriptor to its original target"
            );
        }
    }
}

fn dup2(dst: BorrowedFd<'_>, src: BorrowedFd<'_>) -> io::Result<()> {
    // SAFETY: Both file descriptors are valid because they are obtained using `as_raw_fd`
    // from valid io objects.
    unsafe {
        if libc::dup2(dst.as_raw_fd(), src.as_raw_fd()) >= 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

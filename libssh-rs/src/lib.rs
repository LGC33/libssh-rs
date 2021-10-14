//! This crate provides ergonomic bindings to the functions
//! provided by [libssh](https://libssh.org), a library that provides
//! an implementation of the SSH 2 protocol.  It is distinct from the
//! `ssh2` rust crate which uses [libssh2](https://www.libssh2.org),
//! which is an unrelated project that implements similar functionality!

/// Re-exporting the underlying unsafe API, should you need it
pub use libssh_rs_sys as sys;

use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_uint, c_ulong};
#[cfg(unix)]
use std::os::unix::io::RawFd as RawSocket;
#[cfg(windows)]
use std::os::windows::io::RawSocket;
use std::sync::Arc;
use std::sync::Once;
use std::time::Duration;

mod channel;
mod error;

pub use crate::channel::*;
pub use crate::error::*;

struct LibraryState {}
impl LibraryState {
    pub fn new() -> Option<Self> {
        let res = unsafe { sys::ssh_init() };
        if res != sys::SSH_OK as i32 {
            None
        } else {
            Some(Self {})
        }
    }
}
impl Drop for LibraryState {
    fn drop(&mut self) {
        unsafe { sys::ssh_finalize() };
    }
}

static INIT: Once = Once::new();
static mut LIB: Option<LibraryState> = None;

fn initialize() -> SshResult<()> {
    INIT.call_once(|| unsafe {
        LIB = LibraryState::new();
    });
    if unsafe { LIB.is_none() } {
        Err(Error::fatal("ssh_init failed"))
    } else {
        Ok(())
    }
}

pub(crate) struct SessionHolder {
    sess: sys::ssh_session,
}
unsafe impl Send for SessionHolder {}

impl std::ops::Deref for SessionHolder {
    type Target = sys::ssh_session;
    fn deref(&self) -> &sys::ssh_session {
        &self.sess
    }
}

impl Drop for SessionHolder {
    fn drop(&mut self) {
        unsafe {
            sys::ssh_free(self.sess);
        }
    }
}

impl SessionHolder {
    fn last_error(&self) -> Option<Error> {
        let code = unsafe { sys::ssh_get_error_code(self.sess as _) } as sys::ssh_error_types_e;
        if code == sys::ssh_error_types_e_SSH_NO_ERROR {
            return None;
        }

        let reason = unsafe { sys::ssh_get_error(self.sess as _) };
        let reason = if reason.is_null() {
            String::new()
        } else {
            unsafe { CStr::from_ptr(reason) }
                .to_string_lossy()
                .to_string()
        };

        if code == sys::ssh_error_types_e_SSH_REQUEST_DENIED {
            Some(Error::RequestDenied(reason))
        } else {
            Some(Error::Fatal(reason))
        }
    }

    fn basic_status(&self, res: i32, what: &str) -> SshResult<()> {
        if res == sys::SSH_OK as i32 {
            Ok(())
        } else if res == sys::SSH_AGAIN {
            Err(Error::TryAgain)
        } else if let Some(err) = self.last_error() {
            Err(err)
        } else {
            Err(Error::fatal(what))
        }
    }

    fn blocking_flush(&self, timeout: Option<Duration>) -> SshResult<()> {
        let timeout = match timeout {
            Some(t) => t.as_millis() as c_int,
            None => -1,
        };
        let res = unsafe { sys::ssh_blocking_flush(self.sess, timeout) };
        self.basic_status(res, "blocking_flush")
    }
}

/// A Session represents the state needed to make a connection to
/// a remote host.
///
/// You need at least one Session per target host.
/// A given session can open multiple `Channel`s to perform multiple actions
/// on a given target host.
pub struct Session {
    sess: Arc<SessionHolder>,
}

impl Session {
    /// Create a new Session.
    pub fn new() -> SshResult<Self> {
        initialize()?;
        let sess = unsafe { sys::ssh_new() };
        if sess.is_null() {
            Err(Error::fatal("ssh_new failed"))
        } else {
            Ok(Self {
                sess: Arc::new(SessionHolder { sess }),
            })
        }
    }

    /// Create a new channel.
    /// Channels are used to handle I/O for commands and forwarded streams.
    pub fn new_channel(&self) -> SshResult<Channel> {
        let chan = unsafe { sys::ssh_channel_new(**self.sess) };
        if chan.is_null() {
            if let Some(err) = self.last_error() {
                Err(err)
            } else {
                Err(Error::fatal("ssh_channel_new failed"))
            }
        } else {
            Ok(Channel {
                sess: Arc::clone(&self.sess),
                chan,
            })
        }
    }

    /// Blocking flush of the outgoing buffer.
    pub fn blocking_flush(&self, timeout: Option<Duration>) -> SshResult<()> {
        self.sess.blocking_flush(timeout)
    }

    /// Disconnect from a session (client or server). The session can then be reused to open a new session.
    pub fn disconnect(&self) {
        unsafe { sys::ssh_disconnect(**self.sess) };
    }

    /// Connect to the configured remote host
    pub fn connect(&self) -> SshResult<()> {
        let res = unsafe { sys::ssh_connect(**self.sess) };
        self.sess.basic_status(res, "ssh_connect failed")
    }

    /// Check if the servers public key for the connected session is known.
    /// This checks if we already know the public key of the server we want
    /// to connect to. This allows to detect if there is a MITM attack going
    /// on of if there have been changes on the server we don't know about.
    pub fn is_known_server(&self) -> SshResult<KnownHosts> {
        match unsafe { sys::ssh_session_is_known_server(**self.sess) } {
            sys::ssh_known_hosts_e_SSH_KNOWN_HOSTS_NOT_FOUND => Ok(KnownHosts::NotFound),
            sys::ssh_known_hosts_e_SSH_KNOWN_HOSTS_UNKNOWN => Ok(KnownHosts::Unknown),
            sys::ssh_known_hosts_e_SSH_KNOWN_HOSTS_OK => Ok(KnownHosts::Ok),
            sys::ssh_known_hosts_e_SSH_KNOWN_HOSTS_CHANGED => Ok(KnownHosts::Changed),
            sys::ssh_known_hosts_e_SSH_KNOWN_HOSTS_OTHER => Ok(KnownHosts::Other),
            sys::ssh_known_hosts_e_SSH_KNOWN_HOSTS_ERROR | _ => {
                if let Some(err) = self.last_error() {
                    Err(err)
                } else {
                    Err(Error::fatal("unknown error in ssh_session_is_known_server"))
                }
            }
        }
    }

    /// Add the current connected server to the user known_hosts file.
    /// This adds the currently connected server to the known_hosts file
    /// by appending a new line at the end. The global known_hosts file
    /// is considered read-only so it is not touched by this function.
    pub fn update_known_hosts_file(&self) -> SshResult<()> {
        let res = unsafe { sys::ssh_session_update_known_hosts(**self.sess) };

        if res == sys::SSH_OK as i32 {
            Ok(())
        } else if let Some(err) = self.last_error() {
            Err(err)
        } else {
            Err(Error::fatal("error updating known hosts file"))
        }
    }

    fn last_error(&self) -> Option<Error> {
        self.sess.last_error()
    }

    /// Parse the ssh config file.
    /// This should be the last call of all options, it may overwrite options
    /// which are already set.
    /// It requires that the `SshOption::Hostname` is already set.
    /// if `file_name` is None the default `~/.ssh/config` will be used.
    pub fn options_parse_config(&self, file_name: Option<&str>) -> SshResult<()> {
        let file_name = opt_str_to_cstring(file_name);
        let res =
            unsafe { sys::ssh_options_parse_config(**self.sess, opt_cstring_to_cstr(&file_name)) };
        if res == 0 {
            Ok(())
        } else if let Some(err) = self.last_error() {
            Err(err)
        } else {
            Err(Error::Fatal(format!(
                "error parsing config file: {:?}",
                file_name
            )))
        }
    }

    /// Returns the user name that will be used to authenticate with the remote host
    pub fn get_user_name(&self) -> SshResult<String> {
        let mut name = std::ptr::null_mut();
        let res = unsafe {
            sys::ssh_options_get(**self.sess, sys::ssh_options_e::SSH_OPTIONS_USER, &mut name)
        };
        if res != sys::SSH_OK as i32 || name.is_null() {
            if let Some(err) = self.last_error() {
                Err(err)
            } else {
                Err(Error::fatal("error getting user name"))
            }
        } else {
            let user_name = unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .to_string();
            unsafe { sys::ssh_string_free_char(name) };
            Ok(user_name)
        }
    }

    /// Configures the session.
    /// You will need to set at least `SshOption::Hostname` prior to
    /// connecting, in order for libssh to know where to connect.
    pub fn set_option(&self, option: SshOption) -> SshResult<()> {
        let res = match option {
            SshOption::LogLevel(level) => unsafe {
                let level = match level {
                    LogLevel::NoLogging => sys::SSH_LOG_NOLOG,
                    LogLevel::Warning => sys::SSH_LOG_WARNING,
                    LogLevel::Protocol => sys::SSH_LOG_PROTOCOL,
                    LogLevel::Packet => sys::SSH_LOG_PACKET,
                    LogLevel::Functions => sys::SSH_LOG_FUNCTIONS,
                } as u32 as c_int;
                sys::ssh_options_set(
                    **self.sess,
                    sys::ssh_options_e::SSH_OPTIONS_LOG_VERBOSITY,
                    &level as *const _ as _,
                )
            },
            SshOption::Hostname(name) => unsafe {
                let name = CString::new(name)?;
                sys::ssh_options_set(
                    **self.sess,
                    sys::ssh_options_e::SSH_OPTIONS_HOST,
                    name.as_ptr() as _,
                )
            },
            SshOption::BindAddress(name) => unsafe {
                let name = CString::new(name)?;
                sys::ssh_options_set(
                    **self.sess,
                    sys::ssh_options_e::SSH_OPTIONS_BINDADDR,
                    name.as_ptr() as _,
                )
            },
            SshOption::AddIdentity(name) => unsafe {
                let name = CString::new(name)?;
                sys::ssh_options_set(
                    **self.sess,
                    sys::ssh_options_e::SSH_OPTIONS_ADD_IDENTITY,
                    name.as_ptr() as _,
                )
            },
            SshOption::User(name) => unsafe {
                let name = opt_string_to_cstring(name);
                sys::ssh_options_set(
                    **self.sess,
                    sys::ssh_options_e::SSH_OPTIONS_USER,
                    opt_cstring_to_cstr(&name) as _,
                )
            },
            SshOption::SshDir(name) => unsafe {
                let name = opt_string_to_cstring(name);
                sys::ssh_options_set(
                    **self.sess,
                    sys::ssh_options_e::SSH_OPTIONS_SSH_DIR,
                    opt_cstring_to_cstr(&name) as _,
                )
            },
            SshOption::KnownHosts(known_hosts) => unsafe {
                let known_hosts = opt_string_to_cstring(known_hosts);
                sys::ssh_options_set(
                    **self.sess,
                    sys::ssh_options_e::SSH_OPTIONS_KNOWNHOSTS,
                    opt_cstring_to_cstr(&known_hosts) as _,
                )
            },
            SshOption::Port(port) => {
                let port: c_uint = port.into();
                unsafe {
                    sys::ssh_options_set(
                        **self.sess,
                        sys::ssh_options_e::SSH_OPTIONS_PORT,
                        &port as *const _ as _,
                    )
                }
            }
            SshOption::Socket(socket) => unsafe {
                sys::ssh_options_set(
                    **self.sess,
                    sys::ssh_options_e::SSH_OPTIONS_FD,
                    &socket as *const _ as _,
                )
            },
            SshOption::Timeout(duration) => unsafe {
                let micros: c_ulong = duration.as_micros() as c_ulong;
                sys::ssh_options_set(
                    **self.sess,
                    sys::ssh_options_e::SSH_OPTIONS_TIMEOUT_USEC,
                    &micros as *const _ as _,
                )
            },
        };

        if res == 0 {
            Ok(())
        } else if let Some(err) = self.last_error() {
            Err(err)
        } else {
            Err(Error::fatal("failed to set option"))
        }
    }

    /// This function allows you to get a hash of the public key.
    /// You can then print this hash in a human-readable form to the user
    /// so that he is able to verify it.
    /// It is very important that you verify at some moment that the hash
    /// matches a known server. If you don't do it, cryptography wont help
    /// you at making things secure. OpenSSH uses SHA1 to print public key digests.
    pub fn get_server_public_key(&self) -> SshResult<SshKey> {
        let mut key = std::ptr::null_mut();
        let res = unsafe { sys::ssh_get_server_publickey(**self.sess, &mut key) };
        if res == sys::SSH_OK as i32 && !key.is_null() {
            Ok(SshKey { key })
        } else if let Some(err) = self.last_error() {
            Err(err)
        } else {
            Err(Error::fatal("failed to get server public key"))
        }
    }

    fn auth_result(&self, res: sys::ssh_auth_e, what: &str) -> SshResult<AuthStatus> {
        match res {
            sys::ssh_auth_e_SSH_AUTH_SUCCESS => Ok(AuthStatus::Success),
            sys::ssh_auth_e_SSH_AUTH_DENIED => Ok(AuthStatus::Denied),
            sys::ssh_auth_e_SSH_AUTH_PARTIAL => Ok(AuthStatus::Partial),
            sys::ssh_auth_e_SSH_AUTH_INFO => Ok(AuthStatus::Info),
            sys::ssh_auth_e_SSH_AUTH_AGAIN => Ok(AuthStatus::Again),
            sys::ssh_auth_e_SSH_AUTH_ERROR | _ => {
                if let Some(err) = self.last_error() {
                    Err(err)
                } else {
                    Err(Error::fatal(what))
                }
            }
        }
    }

    /// Try to automatically authenticate using public key authentication.
    ///
    /// This will attempt to use an ssh agent if available, and will then
    /// attempt to use your keys/identities from your `~/.ssh` dir.
    ///
    /// `username` should almost always be `None` to use the username as
    /// previously configured via [set_option](#method.set_option) or that
    /// was loaded from the ssh configuration prior to calling
    /// [connect](#method.connect), as most ssh server implementations
    /// do not allow changing the username during authentication.
    ///
    /// The `password` parameter can be used to pre-fill a password to
    /// unlock the private key.  Leaving it set to `None` will cause
    /// libssh to prompt for the passphrase.
    pub fn userauth_public_key_auto(
        &self,
        username: Option<&str>,
        password: Option<&str>,
    ) -> SshResult<AuthStatus> {
        let username = opt_str_to_cstring(username);
        let password = opt_str_to_cstring(password);

        let res = unsafe {
            sys::ssh_userauth_publickey_auto(
                **self.sess,
                opt_cstring_to_cstr(&username),
                opt_cstring_to_cstr(&password),
            )
        };

        self.auth_result(res, "authentication error")
    }

    /// Try to perform `"none"` authentication.
    ///
    /// Typically, the server will not allow `none` auth to succeed, but it has
    /// the side effect of informing the client which authentication methods
    /// are available, so a full-featured client will call this prior to calling
    /// `userauth_list`.
    ///
    /// `username` should almost always be `None` to use the username as
    /// previously configured via [set_option](#method.set_option) or that
    /// was loaded from the ssh configuration prior to calling
    /// [connect](#method.connect), as most ssh server implementations
    /// do not allow changing the username during authentication.
    pub fn userauth_none(&self, username: Option<&str>) -> SshResult<AuthStatus> {
        let username = opt_str_to_cstring(username);
        let res = unsafe { sys::ssh_userauth_none(**self.sess, opt_cstring_to_cstr(&username)) };

        self.auth_result(res, "authentication error")
    }

    /// Returns the permitted `AuthMethods`.
    ///
    /// The list is not available until after [userauth_none](#method.userauth_none)
    /// has been called at least once.
    ///
    /// The list can change in response to authentication events; for example,
    /// after successfully completing pubkey auth, the server may then require
    /// keyboard interactive auth to enter a second authentication factor.
    ///
    /// `username` should almost always be `None` to use the username as
    /// previously configured via [set_option](#method.set_option) or that
    /// was loaded from the ssh configuration prior to calling
    /// [connect](#method.connect), as most ssh server implementations
    /// do not allow changing the username during authentication.
    pub fn userauth_list(&self, username: Option<&str>) -> SshResult<AuthMethods> {
        let username = opt_str_to_cstring(username);
        Ok(unsafe {
            AuthMethods::from_bits_unchecked(sys::ssh_userauth_list(
                **self.sess,
                opt_cstring_to_cstr(&username),
            ) as u32)
        })
    }

    /// After [userauth_keyboard_interactive](#method.userauth_keyboard_interactive)
    /// has been called and returned `AuthStatus::Info`, this method must be called
    /// to discover the prompts to questions that the server needs answered in order
    /// to authenticate the session.
    ///
    /// It is then up to your application to obtain those answers and set them via
    /// [userauth_keyboard_interactive_set_answers](#method.userauth_keyboard_interactive_set_answers).
    pub fn userauth_keyboard_interactive_info(&self) -> SshResult<InteractiveAuthInfo> {
        let name = unsafe { sys::ssh_userauth_kbdint_getname(**self.sess) };
        let name = unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .to_string();

        let instruction = unsafe { sys::ssh_userauth_kbdint_getinstruction(**self.sess) };
        let instruction = unsafe { CStr::from_ptr(instruction) }
            .to_string_lossy()
            .to_string();

        let n_prompts = unsafe { sys::ssh_userauth_kbdint_getnprompts(**self.sess) };
        assert!(n_prompts >= 0);
        let n_prompts = n_prompts as u32;
        let mut prompts = vec![];
        for i in 0..n_prompts {
            let mut echo = 0;
            let prompt = unsafe { sys::ssh_userauth_kbdint_getprompt(**self.sess, i, &mut echo) };

            prompts.push(InteractiveAuthPrompt {
                prompt: unsafe { CStr::from_ptr(prompt) }
                    .to_string_lossy()
                    .to_string(),
                echo: echo != 0,
            });
        }

        Ok(InteractiveAuthInfo {
            name,
            instruction,
            prompts,
        })
    }

    /// After [userauth_keyboard_interactive_info](#method.userauth_keyboard_interactive_info)
    /// has been called, and your application has produced the answers to the prompts,
    /// you must call this method to record those answers.
    ///
    /// You will then need to call
    /// [userauth_keyboard_interactive](#method.userauth_keyboard_interactive) to present
    /// those answers to the server and discover the next stage of authentication.
    pub fn userauth_keyboard_interactive_set_answers(&self, answers: &[String]) -> SshResult<()> {
        for (idx, answer) in answers.iter().enumerate() {
            let answer = CString::new(answer.as_bytes())?;

            let res = unsafe {
                sys::ssh_userauth_kbdint_setanswer(**self.sess, idx as u32, answer.as_ptr())
            };

            if res != 0 {
                if let Some(err) = self.last_error() {
                    return Err(err);
                }
                return Err(Error::fatal("error setting answer"));
            }
        }
        Ok(())
    }

    /// Initiates keyboard-interactive authentication.
    ///
    /// This appears similar to, but is not the same as password authentication.
    /// You should prefer using keyboard-interactive authentication over password
    /// auth.
    ///
    /// `username` should almost always be `None` to use the username as
    /// previously configured via [set_option](#method.set_option) or that
    /// was loaded from the ssh configuration prior to calling
    /// [connect](#method.connect), as most ssh server implementations
    /// do not allow changing the username during authentication.
    ///
    /// `sub_methods` is not documented in the underlying libssh and
    /// should almost always be `None`.
    ///
    /// If the returned `AuthStatus` is `Info`, then your application
    /// should use [userauth_keyboard_interactive_info](#method.userauth_keyboard_interactive_info)
    /// and use the results of that method to prompt the user to answer
    /// the questions sent by the server, then
    /// [userauth_keyboard_interactive_set_answers](#method.userauth_keyboard_interactive_set_answers)
    /// to record the answers, before again calling this method to
    /// present them to the server and determine the next steps.
    pub fn userauth_keyboard_interactive(
        &self,
        username: Option<&str>,
        sub_methods: Option<&str>,
    ) -> SshResult<AuthStatus> {
        let username = opt_str_to_cstring(username);
        let sub_methods = opt_str_to_cstring(sub_methods);

        let res = unsafe {
            sys::ssh_userauth_kbdint(
                **self.sess,
                opt_cstring_to_cstr(&username),
                opt_cstring_to_cstr(&sub_methods),
            )
        };
        self.auth_result(res, "authentication error")
    }

    /// Initiates password based authentication.
    ///
    /// This appears similar to, but is not the same as keyboard-interactive
    /// authentication. You should prefer using keyboard-interactive
    /// authentication over password auth.
    ///
    /// `username` should almost always be `None` to use the username as
    /// previously configured via [set_option](#method.set_option) or that
    /// was loaded from the ssh configuration prior to calling
    /// [connect](#method.connect), as most ssh server implementations
    /// do not allow changing the username during authentication.
    ///
    /// `password` should be a password entered by the user, or otherwise
    /// securely communicated to your application.
    pub fn userauth_password(
        &self,
        username: Option<&str>,
        password: Option<&str>,
    ) -> SshResult<AuthStatus> {
        let username = opt_str_to_cstring(username);
        let password = opt_str_to_cstring(password);
        let res = unsafe {
            sys::ssh_userauth_password(
                **self.sess,
                opt_cstring_to_cstr(&username),
                opt_cstring_to_cstr(&password),
            )
        };
        self.auth_result(res, "authentication error")
    }

    /// Sends the "tcpip-forward" global request to ask the server
    /// to begin listening for inbound connections; this is for
    /// *remote (or reverse) port forwarding*.
    ///
    /// If `bind_address` is None then bind to all interfaces on
    /// the server side.  Otherwise, bind only to the specified address.
    /// If `port` is `0` then the server will pick a port to bind to,
    /// otherwise, will attempt to use the requested port.
    /// Returns the bound port number.
    ///
    /// Later in your program, you will use `Session::accept_forward` to
    /// wait for a forwarded connection from the address you specified.
    pub fn listen_forward(&self, bind_address: Option<&str>, port: u16) -> SshResult<u16> {
        let bind_address = opt_str_to_cstring(bind_address);
        let mut bound_port = 0;
        let res = unsafe {
            sys::ssh_channel_listen_forward(
                **self.sess,
                opt_cstring_to_cstr(&bind_address),
                port as i32,
                &mut bound_port,
            )
        };
        if res == sys::SSH_OK as i32 {
            Ok(bound_port as u16)
        } else if let Some(err) = self.last_error() {
            Err(err)
        } else {
            Err(Error::fatal("error in ssh_channel_listen_forward"))
        }
    }

    /// Accept a remote forwarded connection.
    /// You must have called `Session::listen_forward` previously to set up
    /// remote port forwarding.
    /// Returns a tuple `(destination_port, Channel)`.
    /// The destination port is so that you can distinguish between multiple
    /// remote forwards and corresponds to the port returned from `listen_forward`.
    pub fn accept_forward(&self, timeout: Duration) -> SshResult<(u16, Channel)> {
        let mut port = 0;
        let chan = unsafe {
            sys::ssh_channel_accept_forward(**self.sess, timeout.as_millis() as _, &mut port)
        };
        if chan.is_null() {
            if let Some(err) = self.last_error() {
                Err(err)
            } else {
                Err(Error::TryAgain)
            }
        } else {
            let channel = Channel {
                sess: Arc::clone(&self.sess),
                chan,
            };

            Ok((port as u16, channel))
        }
    }

    /// Returns a tuple of `(read_pending, write_pending)`.
    /// If `read_pending` is true, then your OS polling mechanism
    /// should request a wakeup when the socket is readable.
    /// If `write_pending` is true, then your OS polling mechanism
    /// should request a wakeup when the socket is writable.
    ///
    /// You can use the `AsRawFd` or `AsRawSocket` trait impl
    /// to obtain the socket descriptor for polling purposes.
    pub fn get_poll_state(&self) -> (bool, bool) {
        let state = unsafe { sys::ssh_get_poll_flags(**self.sess) };
        let read_pending = (state & sys::SSH_READ_PENDING as i32) != 0;
        let write_pending = (state & sys::SSH_WRITE_PENDING as i32) != 0;
        (read_pending, write_pending)
    }

    /// Returns `true` if the session is in blocking mode, `false` otherwise.
    pub fn is_blocking(&self) -> bool {
        unsafe { sys::ssh_is_blocking(**self.sess) != 0 }
    }

    /// If `blocking == true` then set the session to block mode, otherwise
    /// set it to non-blocking mode.
    /// In non-blocking mode, a number of methods in the objects associated
    /// with the session can return `Error::TryAgain`.
    pub fn set_blocking(&self, blocking: bool) {
        unsafe { sys::ssh_set_blocking(**self.sess, if blocking { 1 } else { 0 }) }
    }

    /// Returns `true` if this session is in the connected state, `false`
    /// otherwise.
    pub fn is_connected(&self) -> bool {
        unsafe { sys::ssh_is_connected(**self.sess) != 0 }
    }
}

#[cfg(unix)]
impl std::os::unix::io::AsRawFd for Session {
    fn as_raw_fd(&self) -> RawSocket {
        unsafe { sys::ssh_get_fd(**self.sess) }
    }
}

#[cfg(windows)]
impl std::os::windows::io::AsRawSocket for Session {
    fn as_raw_socket(&self) -> RawSocket {
        unsafe { sys::ssh_get_fd(**self.sess) }
    }
}

/// Indicates the disposition of an authentication operation
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthStatus {
    /// You have been fully authenticated and can now move on
    /// to opening channels
    Success,
    /// The authentication attempt failed. Perhaps retry, or
    /// try an alternative auth method.
    Denied,
    /// You've been partially authenticated.  Check `Session::userauth_list`
    /// to determine which methods you should continue with.
    Partial,
    /// There is additional information about how to proceed
    /// with authentication.  For keyboard-interactive auth,
    /// you will need to obtain auth prompts and provide answers
    /// before you can continue.
    Info,
    /// In non-blocking mode, you will need to try again as
    /// the request couldn't be completed without blocking.
    Again,
}

bitflags::bitflags! {
    /// bitflags that indicates permitted authentication methods
    pub struct AuthMethods : u32 {
        /// The `"none"` authentication method is available.
        const NONE = sys::SSH_AUTH_METHOD_NONE;
        /// The `"password"` authentication method is available.
        const PASSWORD = sys::SSH_AUTH_METHOD_PASSWORD;
        /// The `"public-key"` authentication method is available.
        const PUBLIC_KEY = sys::SSH_AUTH_METHOD_PUBLICKEY;
        /// Host-based authentication is available
        const HOST_BASED = sys::SSH_AUTH_METHOD_HOSTBASED;
        /// keyboard-interactive authentication is available
        const INTERACTIVE = sys::SSH_AUTH_METHOD_INTERACTIVE;
        /// GSSAPI authentication is available
        const GSSAPI_MIC = sys::SSH_AUTH_METHOD_GSSAPI_MIC;
    }
}

/// Represents the public key provided by the remote host
pub struct SshKey {
    key: sys::ssh_key,
}

impl Drop for SshKey {
    fn drop(&mut self) {
        unsafe { sys::ssh_key_free(self.key) }
    }
}

impl SshKey {
    /// Returns the public key hash in the requested format.
    /// The hash is returned as binary bytes.
    /// Consider using [get_public_key_hash_hexa](#method.get_public_key_hash_hexa)
    /// to return it in a more human readable format.
    pub fn get_public_key_hash(&self, hash_type: PublicKeyHashType) -> SshResult<Vec<u8>> {
        let mut bytes = std::ptr::null_mut();
        let mut len = 0;
        let res = unsafe {
            sys::ssh_get_publickey_hash(
                self.key,
                match hash_type {
                    PublicKeyHashType::Sha1 => {
                        sys::ssh_publickey_hash_type::SSH_PUBLICKEY_HASH_SHA1
                    }
                    PublicKeyHashType::Md5 => sys::ssh_publickey_hash_type::SSH_PUBLICKEY_HASH_MD5,
                    PublicKeyHashType::Sha256 => {
                        sys::ssh_publickey_hash_type::SSH_PUBLICKEY_HASH_SHA256
                    }
                },
                &mut bytes,
                &mut len,
            )
        };

        if res != 0 || bytes.is_null() {
            Err(Error::fatal("failed to get public key hash"))
        } else {
            let data = unsafe { std::slice::from_raw_parts(bytes, len).to_vec() };
            unsafe {
                sys::ssh_clean_pubkey_hash(&mut bytes);
            }
            Ok(data)
        }
    }

    /// Returns the public key hash in a human readable form
    pub fn get_public_key_hash_hexa(&self, hash_type: PublicKeyHashType) -> SshResult<String> {
        let bytes = self.get_public_key_hash(hash_type)?;
        let hexa = unsafe { sys::ssh_get_hexa(bytes.as_ptr(), bytes.len()) };
        if hexa.is_null() {
            Err(Error::fatal(
                "failed to allocate bytes for hexa representation",
            ))
        } else {
            let res = unsafe { CStr::from_ptr(hexa) }
                .to_string_lossy()
                .to_string();
            unsafe { sys::ssh_string_free_char(hexa) };
            Ok(res)
        }
    }
}

/// Allows configuring the underlying `libssh` debug logging level
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    NoLogging,
    Warning,
    Protocol,
    Packet,
    Functions,
}

/// Allows configuring different aspects of a `Session`.
/// You always need to set at least `SshOption::Hostname`.
#[derive(Debug)]
pub enum SshOption {
    /// The hostname or ip address to connect to
    Hostname(String),

    /// The port to connect to
    Port(u16),

    LogLevel(LogLevel),

    /// The pre-opened socket.
    /// You don't typically need to provide this.
    /// Don't forget to set the hostname as the hostname is used as a
    /// key in the known_host mechanism.
    Socket(RawSocket),

    /// The address to bind the client to
    BindAddress(String),

    /// The username for authentication
    /// If the value is None, the username is set to the default username.
    User(Option<String>),

    /// Set the ssh directory
    /// If the value is None, the directory is set to the default ssh directory.
    /// The ssh directory is used for files like known_hosts and identity (private and public key). It may include "%s" which will be replaced by the user home directory.
    SshDir(Option<String>),

    /// Set the known hosts file name
    /// If the value is None, the directory is set to the default known hosts file, normally ~/.ssh/known_hosts.
    /// The known hosts file is used to certify remote hosts are genuine. It may include "%d" which will be replaced by the user home directory.
    KnownHosts(Option<String>),

    /// Add a new identity file (const char *, format string) to the identity list.
    /// By default identity, id_dsa and id_rsa are checked.
    /// The identity used to authenticate with public key will be prepended to the list. It may include "%s" which will be replaced by the user home directory.
    AddIdentity(String),

    /// Set a timeout for the connection
    Timeout(Duration),
}

/// Indicates the state of known-host matching, an important set
/// to detect and avoid MITM attacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownHosts {
    /// The known host file does not exist. The host is thus unknown. File will be created if host key is accepted.
    NotFound,
    /// The server is unknown. User should confirm the public key hash is correct.
    Unknown,
    /// The server is known and has not changed.
    Ok,
    /// The server key has changed. Either you are under attack or the administrator changed the key. You HAVE to warn the user about a possible attack.
    Changed,
    /// The server gave use a key of a type while we had an other type recorded. It is a possible attack.
    Other,
}

/// The type of hash to use when inspecting a public key fingerprint
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublicKeyHashType {
    Sha1,
    Md5,
    Sha256,
}

/// Represents a question prompt in keyboard-interactive auth
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveAuthPrompt {
    /// The prompt to show to the user
    pub prompt: String,
    /// If `true`, echo the user's answer to the screen.
    /// If `false`, conceal it, as it is secret/sensitive.
    pub echo: bool,
}

/// Represents the overall set of instructions in keyboard-interactive auth
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractiveAuthInfo {
    /// An overall set of instructions.
    /// May be empty.
    pub instruction: String,
    /// The session name.
    /// May be empty.
    pub name: String,
    /// The set of prompts for information that need answers before
    /// authentication can succeed.
    pub prompts: Vec<InteractiveAuthPrompt>,
}

/// A utility function that will prompt the user for input
/// via the console/tty.
///
/// `prompt` is the text to show to the user.
/// `default_value` can be used to pre-set the answer, allowing the
/// user to simply press enter.
///
/// `echo`, if `true`, means to show the user's answer on the screen
/// as they type it.  If `false`, means to conceal it.
///
/// `verify`, if `true`, will ask the user for their input twice in
/// order to confirm that they provided the same text both times.
/// This is useful when creating a password and `echo == false`.
pub fn get_input(
    prompt: &str,
    default_value: Option<&str>,
    echo: bool,
    verify: bool,
) -> Option<String> {
    const BUF_LEN: usize = 128;
    let mut buf = [0u8; BUF_LEN];

    if let Some(def) = default_value {
        let def = def.as_bytes();
        let len = buf.len().min(def.len());
        buf[0..len].copy_from_slice(&def[0..len]);
    }

    let prompt = CString::new(prompt).ok()?;

    let res = unsafe {
        sys::ssh_getpass(
            prompt.as_ptr(),
            buf.as_mut_ptr() as *mut _,
            buf.len(),
            if echo { 1 } else { 0 },
            if verify { 1 } else { 0 },
        )
    };

    if res == 0 {
        Some(
            unsafe { CStr::from_ptr(buf.as_ptr() as *const _) }
                .to_string_lossy()
                .to_string(),
        )
    } else {
        None
    }
}

fn opt_str_to_cstring(s: Option<&str>) -> Option<CString> {
    s.and_then(|s| CString::new(s).ok())
}

fn opt_string_to_cstring(s: Option<String>) -> Option<CString> {
    s.and_then(|s| CString::new(s).ok())
}

fn opt_cstring_to_cstr(s: &Option<CString>) -> *const i8 {
    match s {
        Some(s) => s.as_ptr(),
        None => std::ptr::null(),
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn init() {
        let sess = Session::new().unwrap();
        assert!(!sess.is_connected());
        assert!(sess.last_error().is_none());
        assert_eq!(sess.connect(), Err(Error::fatal("Hostname required")));
    }
}

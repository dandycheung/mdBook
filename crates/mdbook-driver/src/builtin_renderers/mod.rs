//! Built-in renderers.
//!
//! The HTML renderer can be found in the [`mdbook_html`] crate.

use anyhow::{Context, Result, bail};
use log::{error, info, trace, warn};
use mdbook_renderer::{RenderContext, Renderer};
use shlex::Shlex;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub use self::markdown_renderer::MarkdownRenderer;

mod markdown_renderer;

/// A generic renderer which will shell out to an arbitrary executable.
///
/// # Rendering Protocol
///
/// When the renderer's `render()` method is invoked, `CmdRenderer` will spawn
/// the `cmd` as a subprocess. The `RenderContext` is passed to the subprocess
/// as a JSON string (using `serde_json`).
///
/// > **Note:** The command used doesn't necessarily need to be a single
/// > executable (i.e. `/path/to/renderer`). The `cmd` string lets you pass
/// > in command line arguments, so there's no reason why it couldn't be
/// > `python /path/to/renderer --from mdbook --to epub`.
///
/// Anything the subprocess writes to `stdin` or `stdout` will be passed through
/// to the user. While this gives the renderer maximum flexibility to output
/// whatever it wants, to avoid spamming users it is recommended to avoid
/// unnecessary output.
///
/// To help choose the appropriate output level, the `RUST_LOG` environment
/// variable will be passed through to the subprocess, if set.
///
/// If the subprocess wishes to indicate that rendering failed, it should exit
/// with a non-zero return code.
#[derive(Debug, Clone, PartialEq)]
pub struct CmdRenderer {
    name: String,
    cmd: String,
}

impl CmdRenderer {
    /// Create a new `CmdRenderer` which will invoke the provided `cmd` string.
    pub fn new(name: String, cmd: String) -> CmdRenderer {
        CmdRenderer { name, cmd }
    }

    fn compose_command(&self, root: &Path, destination: &Path) -> Result<Command> {
        let mut words = Shlex::new(&self.cmd);
        let exe = match words.next() {
            Some(e) => PathBuf::from(e),
            None => bail!("Command string was empty"),
        };

        let exe = if exe.components().count() == 1 {
            // Search PATH for the executable.
            exe
        } else {
            // Relative paths are preferred to be relative to the book root.
            let abs_exe = root.join(&exe);
            if abs_exe.exists() {
                abs_exe
            } else {
                // Historically paths were relative to the destination, but
                // this is not the preferred way.
                let legacy_path = destination.join(&exe);
                if legacy_path.exists() {
                    warn!(
                        "Renderer command `{}` uses a path relative to the \
                        renderer output directory `{}`. This was previously \
                        accepted, but has been deprecated. Relative executable \
                        paths should be relative to the book root.",
                        exe.display(),
                        destination.display()
                    );
                    legacy_path
                } else {
                    // Let this bubble through to later be handled by
                    // handle_render_command_error.
                    abs_exe
                }
            }
        };

        let mut cmd = Command::new(exe);

        for arg in words {
            cmd.arg(arg);
        }

        Ok(cmd)
    }
}

impl CmdRenderer {
    fn handle_render_command_error(&self, ctx: &RenderContext, error: io::Error) -> Result<()> {
        if let ErrorKind::NotFound = error.kind() {
            // Look for "output.{self.name}.optional".
            // If it exists and is true, treat this as a warning.
            // Otherwise, fail the build.

            let optional_key = format!("output.{}.optional", self.name);

            let is_optional = match ctx.config.get(&optional_key) {
                Ok(Some(value)) => value,
                Err(e) => bail!("expected bool for `{optional_key}`: {e}"),
                Ok(None) => false,
            };

            if is_optional {
                warn!(
                    "The command `{}` for backend `{}` was not found, \
                    but was marked as optional.",
                    self.cmd, self.name
                );
                return Ok(());
            } else {
                error!(
                    "The command `{0}` wasn't found, is the \"{1}\" backend installed? \
                    If you want to ignore this error when the \"{1}\" backend is not installed, \
                    set `optional = true` in the `[output.{1}]` section of the book.toml configuration file.",
                    self.cmd, self.name
                );
            }
        }
        Err(error).with_context(|| "Unable to start the backend")?
    }
}

impl Renderer for CmdRenderer {
    fn name(&self) -> &str {
        &self.name
    }

    fn render(&self, ctx: &RenderContext) -> Result<()> {
        info!("Invoking the \"{}\" renderer", self.name);

        let _ = fs::create_dir_all(&ctx.destination);

        let mut child = match self
            .compose_command(&ctx.root, &ctx.destination)?
            .stdin(Stdio::piped())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .current_dir(&ctx.destination)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => return self.handle_render_command_error(ctx, e),
        };

        let mut stdin = child.stdin.take().expect("Child has stdin");
        if let Err(e) = serde_json::to_writer(&mut stdin, &ctx) {
            // Looks like the backend hung up before we could finish
            // sending it the render context. Log the error and keep going
            warn!("Error writing the RenderContext to the backend, {}", e);
        }

        // explicitly close the `stdin` file handle
        drop(stdin);

        let status = child
            .wait()
            .with_context(|| "Error waiting for the backend to complete")?;

        trace!("{} exited with output: {:?}", self.cmd, status);

        if !status.success() {
            error!("Renderer exited with non-zero return code.");
            bail!("The \"{}\" renderer failed", self.name);
        } else {
            Ok(())
        }
    }
}

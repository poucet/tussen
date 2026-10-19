//! Which `hub.toml` the hub uses, and where edits to it go.
//!
//! Resolution, first hit wins:
//! 1. `--config <path>`;
//! 2. the user config, `$XDG_CONFIG_HOME/tussen/hub.toml` (default
//!    `~/.config/tussen/hub.toml`, on macOS too — the CLI-tool convention);
//! 3. the nearest `hub.toml` walking up from the working directory — the
//!    repo's shipped example;
//! 4. that same example compiled into the binary ([`SHIPPED`]), so an
//!    installed `tussen-hub run` works from anywhere.
//!
//! Neither example is ever written: Save from either goes to the user
//! config instead (see [`ConfigFile::save_target`]).

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Context;

/// The shipped example, compiled in: `config init` outside a checkout,
/// and the last resort of [`ConfigFile::resolve`].
pub const SHIPPED: &str = include_str!("../../hub.toml");

/// Where the config in use came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Flag,
    User,
    Repo,
    /// No file at all: [`SHIPPED`].
    Builtin,
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Source::Flag => "--config",
            Source::User => "user",
            Source::Repo => "repo",
            Source::Builtin => "built-in",
        })
    }
}

/// The environment resolution reads, passed in so tests never touch the
/// real home directory.
#[derive(Clone, Debug, Default)]
pub struct Env {
    pub xdg_config_home: Option<PathBuf>,
    pub home: Option<PathBuf>,
    pub cwd: PathBuf,
}

impl Env {
    pub fn from_process() -> anyhow::Result<Env> {
        Ok(Env {
            xdg_config_home: std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            home: std::env::var_os("HOME").map(PathBuf::from),
            cwd: std::env::current_dir()?,
        })
    }

    /// `$XDG_CONFIG_HOME/tussen/hub.toml`, else `$HOME/.config/tussen/hub.toml`.
    /// A relative or empty `XDG_CONFIG_HOME` is ignored, as the XDG spec says.
    pub fn user_path(&self) -> Option<PathBuf> {
        let base = match &self.xdg_config_home {
            Some(x) if x.is_absolute() => x.clone(),
            _ => self.home.as_ref().filter(|h| !h.as_os_str().is_empty())?.join(".config"),
        };
        Some(base.join("tussen").join("hub.toml"))
    }

    /// The nearest `hub.toml` walking up from the working directory.
    pub fn repo_path(&self) -> Option<PathBuf> {
        self.cwd.ancestors().map(|d| d.join("hub.toml")).find(|p| p.is_file())
    }
}

/// The config file in use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfigFile {
    /// The file read. For [`Source::Builtin`] there is none: this is the
    /// user path Save would create (empty when HOME is unset).
    pub path: PathBuf,
    pub source: Source,
    /// Where an example's (repo or built-in) edits go.
    pub user_path: Option<PathBuf>,
}

impl fmt::Display for ConfigFile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.source, &self.user_path) {
            (Source::Builtin, Some(user)) => {
                write!(f, "(built-in example; tussen-hub config init writes {})", user.display())
            }
            (Source::Builtin, None) => f.write_str("(built-in example; HOME is unset, so no user config)"),
            _ => write!(f, "{} ({})", self.path.display(), self.source),
        }
    }
}

impl ConfigFile {
    pub fn resolve(flag: Option<PathBuf>, env: &Env) -> anyhow::Result<ConfigFile> {
        let user_path = env.user_path();
        let (path, source) = if let Some(p) = flag {
            (p, Source::Flag)
        } else if let Some(p) = user_path.clone().filter(|p| p.is_file()) {
            (p, Source::User)
        } else if let Some(p) = env.repo_path() {
            (p, Source::Repo)
        } else {
            (user_path.clone().unwrap_or_default(), Source::Builtin)
        };
        Ok(ConfigFile { path, source, user_path })
    }

    /// The config text: the file's, or [`SHIPPED`] for the built-in.
    pub fn text(&self) -> anyhow::Result<String> {
        match self.source {
            Source::Builtin => Ok(SHIPPED.to_owned()),
            _ => std::fs::read_to_string(&self.path)
                .with_context(|| format!("reading config {}", self.path.display())),
        }
    }

    /// Where Save writes. An example (repo or built-in) is first copied,
    /// comments and all, to the user path, and `self` switches to it — the
    /// example stays untouched. Refuses if the user file appeared since the
    /// hub started (copying over it would lose its edits).
    pub fn save_target(&mut self) -> anyhow::Result<&Path> {
        if matches!(self.source, Source::Repo | Source::Builtin) {
            let user = self.user_path.clone().context("no user config path (HOME unset); start with --config")?;
            if user.exists() {
                anyhow::bail!("{} appeared after the hub started; restart the hub to use it", user.display());
            }
            write_new(&user, &self.text()?)?;
            log::info!("config: the {} example stays as shipped; edits go to {}", self.source, user.display());
            self.path = user;
            self.source = Source::User;
            log::info!("config: {self}");
        }
        Ok(&self.path)
    }
}

/// `tussen-hub config init`: put a copy of `from` (the shipped example
/// when `None`) at `to`. Never overwrites.
pub fn init(from: Option<&Path>, to: &Path) -> anyhow::Result<()> {
    if to.exists() {
        anyhow::bail!("{} already exists; not overwriting it", to.display());
    }
    match from {
        Some(from) => copy_new(from, to),
        None => write_new(to, SHIPPED),
    }
}

fn copy_new(from: &Path, to: &Path) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(from).with_context(|| format!("reading {}", from.display()))?;
    write_new(to, &text)
}

fn write_new(to: &Path, text: &str) -> anyhow::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(to)
        .with_context(|| format!("creating {}", to.display()))?;
    f.write_all(text.as_bytes()).with_context(|| format!("writing {}", to.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fresh temp dir per test: `<tmp>/tussen-cfg-<pid>-<name>`.
    pub(crate) fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tussen-cfg-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// `<root>/repo/sub` as the cwd with `<root>/repo/hub.toml`, and
    /// `<root>/xdg` as XDG_CONFIG_HOME (empty).
    pub(crate) fn layout(root: &Path) -> Env {
        let cwd = root.join("repo").join("sub");
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(root.join("repo").join("hub.toml"), "# repo\n").unwrap();
        Env { xdg_config_home: Some(root.join("xdg")), home: Some(root.join("home")), cwd }
    }

    #[test]
    fn the_user_path_follows_xdg_then_home() {
        let mut env = Env { xdg_config_home: Some("/x".into()), home: Some("/h".into()), cwd: "/".into() };
        assert_eq!(env.user_path().unwrap(), Path::new("/x/tussen/hub.toml"));
        env.xdg_config_home = Some("relative".into());
        assert_eq!(env.user_path().unwrap(), Path::new("/h/.config/tussen/hub.toml"));
        env.xdg_config_home = Some("".into());
        assert_eq!(env.user_path().unwrap(), Path::new("/h/.config/tussen/hub.toml"));
        env.xdg_config_home = None;
        assert_eq!(env.user_path().unwrap(), Path::new("/h/.config/tussen/hub.toml"));
        env.home = None;
        assert_eq!(env.user_path(), None);
    }

    #[test]
    fn resolution_is_flag_then_user_then_repo_then_built_in() {
        let root = scratch("order");
        let env = layout(&root);
        let repo = root.join("repo").join("hub.toml");
        let user = root.join("xdg").join("tussen").join("hub.toml");

        let c = ConfigFile::resolve(None, &env).unwrap();
        assert_eq!((c.path.as_path(), c.source), (repo.as_path(), Source::Repo));
        assert_eq!(c.user_path.as_deref(), Some(user.as_path()));

        std::fs::create_dir_all(user.parent().unwrap()).unwrap();
        std::fs::write(&user, "# user\n").unwrap();
        let c = ConfigFile::resolve(None, &env).unwrap();
        assert_eq!((c.path.as_path(), c.source), (user.as_path(), Source::User));
        assert_eq!(c.to_string(), format!("{} (user)", user.display()));

        let c = ConfigFile::resolve(Some("/elsewhere.toml".into()), &env).unwrap();
        assert_eq!((c.path.as_path(), c.source), (Path::new("/elsewhere.toml"), Source::Flag));

        // Without XDG_CONFIG_HOME: ~/.config.
        let env = Env { xdg_config_home: None, ..env };
        let c = ConfigFile::resolve(None, &env).unwrap();
        assert_eq!(c.source, Source::Repo);
        assert_eq!(c.user_path.unwrap(), root.join("home/.config/tussen/hub.toml"));

        // Nothing anywhere: the built-in example, which names where its
        // edits would go.
        let env = Env { cwd: root.join("home"), ..env };
        std::fs::create_dir_all(&env.cwd).unwrap();
        if env.repo_path().is_none() {
            let c = ConfigFile::resolve(None, &env).unwrap();
            let user = root.join("home/.config/tussen/hub.toml");
            assert_eq!(c.source, Source::Builtin);
            assert_eq!(c.user_path.as_deref(), Some(user.as_path()));
            assert_eq!(
                c.to_string(),
                format!("(built-in example; tussen-hub config init writes {})", user.display())
            );
            assert_eq!(c.text().unwrap(), SHIPPED);
            let c = ConfigFile::resolve(None, &Env { home: None, ..env }).unwrap();
            assert_eq!(c.source, Source::Builtin);
            assert!(c.to_string().contains("HOME is unset"), "{c}");
        }
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn the_built_in_example_is_a_working_config() {
        let c = ConfigFile { path: PathBuf::new(), source: Source::Builtin, user_path: None };
        let config = crate::config::Config::from_file(&c).unwrap();
        assert_eq!(config.osc.listen_port, 9000);
        assert!(config.console.enabled);
    }

    #[test]
    fn init_copies_and_refuses_to_overwrite() {
        let root = scratch("init");
        let env = layout(&root);
        let user = env.user_path().unwrap();
        let repo = env.repo_path().unwrap();
        init(Some(&repo), &user).unwrap();
        assert_eq!(std::fs::read_to_string(&user).unwrap(), "# repo\n");
        std::fs::write(&user, "# mine\n").unwrap();
        let e = init(Some(&repo), &user).unwrap_err().to_string();
        assert!(e.contains("already exists"), "{e}");
        assert_eq!(std::fs::read_to_string(&user).unwrap(), "# mine\n");
        // Outside a checkout: the shipped example.
        let other = root.join("other").join("hub.toml");
        init(None, &other).unwrap();
        assert_eq!(std::fs::read_to_string(&other).unwrap(), SHIPPED);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn save_target_moves_a_repo_fallback_to_the_user_path() {
        let root = scratch("target");
        let env = layout(&root);
        let mut c = ConfigFile::resolve(None, &env).unwrap();
        let user = c.user_path.clone().unwrap();
        assert_eq!(c.save_target().unwrap(), user.as_path());
        assert_eq!(c.source, Source::User);
        assert_eq!(std::fs::read_to_string(&user).unwrap(), "# repo\n");
        // Already the user file: stays put.
        assert_eq!(c.save_target().unwrap(), user.as_path());

        // A user file that appeared after start is not clobbered.
        let mut c = ConfigFile { path: env.repo_path().unwrap(), source: Source::Repo, user_path: Some(user.clone()) };
        assert!(c.save_target().is_err());
        assert_eq!(c.source, Source::Repo);

        // The built-in example: SHIPPED goes to the user path first.
        std::fs::remove_file(&user).unwrap();
        let mut c = ConfigFile { path: user.clone(), source: Source::Builtin, user_path: Some(user.clone()) };
        assert_eq!(c.save_target().unwrap(), user.as_path());
        assert_eq!(c.source, Source::User);
        assert_eq!(std::fs::read_to_string(&user).unwrap(), SHIPPED);
        let mut c = ConfigFile { path: user.clone(), source: Source::Builtin, user_path: Some(user.clone()) };
        assert!(c.save_target().is_err(), "clobbered a user file that appeared after start");
        let mut c = ConfigFile { path: PathBuf::new(), source: Source::Builtin, user_path: None };
        assert!(c.save_target().is_err());

        // --config writes where it points.
        let mut c = ConfigFile { path: "/x.toml".into(), source: Source::Flag, user_path: Some(user) };
        assert_eq!(c.save_target().unwrap(), Path::new("/x.toml"));
        std::fs::remove_dir_all(&root).unwrap();
    }
}

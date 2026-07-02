//! Broker configuration, written by the installer to `/etc/agentd/broker.conf`
//! and read by the root broker at startup. Plain `key = value` lines (no TOML
//! dependency in the broker's tiny surface); pure parsing, tested everywhere.

use super::pool::SandboxUser;

/// Parsed broker config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrokerConfig {
    /// The single unprivileged uid allowed to connect (the daemon's uid, from
    /// `SUDO_UID` at install). Every accepted connection is `getpeereid`-checked
    /// against this.
    pub daemon_uid: u32,
    /// The sandbox user pool.
    pub users: Vec<SandboxUser>,
}

/// Parse error with line context.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("missing required key `{0}`")]
    Missing(&'static str),
    #[error("bad value for `{key}`: {val}")]
    BadValue { key: String, val: String },
    #[error("no sandbox users configured")]
    NoUsers,
}

impl BrokerConfig {
    /// Parse the config text. Format:
    /// ```text
    /// daemon_uid = 501
    /// sandbox_user = 700 _agentd_sbx0
    /// sandbox_user = 701 _agentd_sbx1
    /// ```
    /// Blank lines and `#` comments ignored.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let mut daemon_uid = None;
        let mut users = Vec::new();
        for raw in text.lines() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let (key, val) = match line.split_once('=') {
                Some((k, v)) => (k.trim(), v.trim()),
                None => continue,
            };
            match key {
                "daemon_uid" => {
                    daemon_uid = Some(val.parse().map_err(|_| ConfigError::BadValue {
                        key: key.into(),
                        val: val.into(),
                    })?);
                }
                "sandbox_user" => {
                    let mut it = val.split_whitespace();
                    let uid = it
                        .next()
                        .and_then(|s| s.parse().ok())
                        .ok_or_else(|| ConfigError::BadValue {
                            key: key.into(),
                            val: val.into(),
                        })?;
                    let name = it
                        .next()
                        .ok_or_else(|| ConfigError::BadValue {
                            key: key.into(),
                            val: val.into(),
                        })?
                        .to_string();
                    users.push(SandboxUser { uid, name });
                }
                _ => {}
            }
        }
        let daemon_uid = daemon_uid.ok_or(ConfigError::Missing("daemon_uid"))?;
        if users.is_empty() {
            return Err(ConfigError::NoUsers);
        }
        Ok(BrokerConfig { daemon_uid, users })
    }

    /// Render config text (installer side).
    pub fn render(&self) -> String {
        let mut s = format!("daemon_uid = {}\n", self.daemon_uid);
        for u in &self.users {
            s.push_str(&format!("sandbox_user = {} {}\n", u.uid, u.name));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_config() {
        let c = BrokerConfig::parse(
            "# broker\ndaemon_uid = 501\nsandbox_user = 700 _agentd_sbx0\nsandbox_user = 701 _agentd_sbx1\n",
        )
        .unwrap();
        assert_eq!(c.daemon_uid, 501);
        assert_eq!(c.users.len(), 2);
        assert_eq!(c.users[0], SandboxUser { uid: 700, name: "_agentd_sbx0".into() });
    }

    #[test]
    fn render_parse_roundtrip() {
        let c = BrokerConfig {
            daemon_uid: 501,
            users: vec![SandboxUser { uid: 700, name: "_agentd_sbx0".into() }],
        };
        assert_eq!(BrokerConfig::parse(&c.render()).unwrap(), c);
    }

    #[test]
    fn missing_daemon_uid_errors() {
        let e = BrokerConfig::parse("sandbox_user = 700 _agentd_sbx0\n").unwrap_err();
        assert_eq!(e, ConfigError::Missing("daemon_uid"));
    }

    #[test]
    fn no_users_errors() {
        let e = BrokerConfig::parse("daemon_uid = 501\n").unwrap_err();
        assert_eq!(e, ConfigError::NoUsers);
    }

    #[test]
    fn bad_uid_errors() {
        let e = BrokerConfig::parse("daemon_uid = notanumber\n").unwrap_err();
        assert!(matches!(e, ConfigError::BadValue { .. }));
    }

    #[test]
    fn comments_and_blanks_ignored() {
        let c = BrokerConfig::parse("\n\n# a comment\ndaemon_uid = 1 # inline\nsandbox_user = 700 x\n")
            .unwrap();
        assert_eq!(c.daemon_uid, 1);
    }
}

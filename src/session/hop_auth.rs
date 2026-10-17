//! The jump hosts of a ProxyJump chain and their own stored credentials.
//!
//! ssh runs every `-J` hop as a separate `ssh` that inherits the destination's
//! askpass environment, so a hop's `user@10.0.4.11's password:` arrives at the
//! same helper as the destination's prompt. The destination's secret must never
//! answer it (#141), and nothing used to answer it at all: the user typed the
//! password of a host whose password SSHub already holds. A hop prompt is now
//! answered with the stored secret of the hop host it names, and only that one.
//!
//! The prompt is the only thing ssh tells us about which hop is asking, so it is
//! matched by what it prints: `user@host's password:` (host as ssh resolved it,
//! the saved host's address) or `Enter passphrase for key '<path>':`. A prompt
//! that matches no hop, or more than one, is answered by nobody: it goes to the
//! user, as before.

use std::sync::Arc;

use super::auth_prompt::PromptClass;
use super::PendingSecret;

/// Fetches a hop's stored secret of the asked class, when the hop asks.
pub type Fetch = Arc<dyn Fn(PromptClass) -> Option<PendingSecret> + Send + Sync>;

/// A saved host used as a jump hop, with a lazy way to get its secret. No
/// `Debug`: nothing here may reach diagnostic formatting.
#[derive(Clone)]
pub struct HopCred {
    /// The saved host's name, for the modal ("hop lb-1").
    pub name: String,
    /// The login ssh prints in the prompt; `None` = whatever ssh defaults to.
    pub user: Option<String>,
    /// The address ssh prints in the prompt.
    pub host: String,
    /// Where a remembered password for this hop is saved.
    pub host_id: Option<i64>,
    /// Where a remembered key passphrase is saved.
    pub identity_id: Option<i64>,
    /// The hop identity's private key, to recognise its passphrase prompt.
    pub key_path: Option<String>,
    fetch: Fetch,
}

impl HopCred {
    pub fn new(
        name: String,
        user: Option<String>,
        host: String,
        host_id: Option<i64>,
        identity_id: Option<i64>,
        key_path: Option<String>,
        fetch: Fetch,
    ) -> Self {
        Self {
            name,
            user,
            host,
            host_id,
            identity_id,
            key_path,
            fetch,
        }
    }

    /// The stored secret for a prompt of `class`, fetched now.
    pub fn secret(&self, class: PromptClass) -> Option<PendingSecret> {
        (self.fetch)(class).filter(|s| class.matches_secret(s))
    }
}

/// A hop's secret already fetched, for ssh children that read it from a staged
/// file (tunnels, exec, broadcast, key push).
#[derive(Clone)]
pub struct HopSecret {
    pub user: Option<String>,
    pub host: String,
    pub key_path: Option<String>,
    pub class: PromptClass,
    pub value: String,
}

impl std::fmt::Debug for HopSecret {
    /// The value never reaches a log or a panic message.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HopSecret")
            .field("user", &self.user)
            .field("host", &self.host)
            .field("class", &self.class)
            .finish_non_exhaustive()
    }
}

impl HopSecret {
    /// Every secret `cred` holds, one per prompt class it can answer.
    pub fn of(cred: &HopCred) -> Vec<HopSecret> {
        [PromptClass::Password, PromptClass::Passphrase]
            .into_iter()
            .filter_map(|class| {
                let secret = cred.secret(class)?;
                Some(HopSecret {
                    user: cred.user.clone(),
                    host: cred.host.clone(),
                    key_path: cred.key_path.clone(),
                    class,
                    value: secret.value().to_string(),
                })
            })
            .collect()
    }
}

/// Who a prompt says is asking.
#[derive(Debug, PartialEq, Eq)]
pub enum Asker<'a> {
    /// `user@host's password:`
    Login { user: &'a str, host: &'a str },
    /// `Enter passphrase for key '<path>':`
    Key(&'a str),
}

/// Read the asker out of an ssh prompt. Only ssh's own two prompts count, and
/// only whole: `user@host's password: ` and `Enter passphrase for key '<path>': `
/// with nothing before them. A hostile jump host can make ssh show any
/// keyboard-interactive text, but ssh puts `(user@host) ` in front of it.
pub fn asker(prompt: &str) -> Option<Asker<'_>> {
    let prompt = prompt.trim_end();
    if let Some(login) = prompt.strip_suffix("'s password:") {
        if login.contains(char::is_whitespace) {
            return None;
        }
        let (user, host) = login.rsplit_once('@')?;
        return (!user.is_empty() && !host.is_empty()).then_some(Asker::Login { user, host });
    }
    let path = prompt
        .strip_prefix("Enter passphrase for key '")?
        .strip_suffix("':")?;
    (!path.is_empty()).then_some(Asker::Key(path))
}

/// The one hop `prompt` names, `None` when it names none or several.
pub fn find<'a>(hops: &'a [HopCred], prompt: &str) -> Option<&'a HopCred> {
    unique(
        hops,
        &asker(prompt)?,
        |h| (h.user.as_deref(), h.host.as_str()),
        |h| h.key_path.as_deref(),
    )
}

/// The one staged secret whose hop and class `prompt` names.
pub fn find_secret<'a>(hops: &'a [HopSecret], prompt: &str) -> Option<&'a HopSecret> {
    let class = PromptClass::classify(prompt);
    let who = asker(prompt)?;
    let same_class: Vec<&HopSecret> = hops.iter().filter(|h| h.class == class).collect();
    unique(
        &same_class,
        &who,
        |h| (h.user.as_deref(), h.host.as_str()),
        |h| h.key_path.as_deref(),
    )
    .copied()
}

fn unique<'a, T>(
    items: &'a [T],
    who: &Asker<'_>,
    login: impl Fn(&T) -> (Option<&str>, &str),
    key: impl Fn(&T) -> Option<&str>,
) -> Option<&'a T> {
    let mut found = None;
    for item in items {
        let hit = match who {
            Asker::Login { user, host } => {
                let (u, h) = login(item);
                h.eq_ignore_ascii_case(host) && u.is_none_or(|u| u == *user)
            }
            Asker::Key(path) => key(item) == Some(*path),
        };
        if hit {
            if found.is_some() {
                // Two hops this prompt could be: answer neither.
                return None;
            }
            found = Some(item);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hop(name: &str, user: Option<&str>, host: &str) -> HopCred {
        let secret = format!("{name}-pw");
        HopCred::new(
            name.into(),
            user.map(String::from),
            host.into(),
            Some(1),
            None,
            None,
            Arc::new(move |class| {
                (class == PromptClass::Password).then(|| PendingSecret::Password(secret.clone()))
            }),
        )
    }

    #[test]
    fn the_asker_is_read_from_what_ssh_prints() {
        assert_eq!(
            asker("ops@10.0.4.11's password: "),
            Some(Asker::Login {
                user: "ops",
                host: "10.0.4.11"
            })
        );
        // Anything around the prompt makes it not ssh's own: a banner line in
        // front, or the `(user@host) ` prefix every keyboard-interactive
        // prompt carries (a server picks that text, so it can name any hop).
        assert_eq!(
            asker("Warning: x\r\njohn@corp.com@10.0.4.12's password: "),
            None
        );
        assert_eq!(asker("(ops@bastion) ops@10.0.4.12's password: "), None);
        assert_eq!(
            asker("john@corp.com@10.0.4.12's password: "),
            Some(Asker::Login {
                user: "john@corp.com",
                host: "10.0.4.12"
            })
        );
        assert_eq!(
            asker("(ops@bastion) Enter passphrase for key '/home/u/.ssh/id': "),
            None
        );
        assert_eq!(asker("Please Enter passphrase for key '/k': "), None);
        assert_eq!(
            asker("Enter passphrase for key '/home/u/.ssh/id_ed25519': "),
            Some(Asker::Key("/home/u/.ssh/id_ed25519"))
        );
        assert_eq!(asker("Verification code:"), None);
        assert_eq!(asker("'s password"), None);
    }

    #[test]
    fn a_prompt_finds_exactly_the_hop_it_names() {
        let hops = [
            hop("lb-1", Some("ops"), "10.0.4.11"),
            hop("lb-2", Some("ops"), "10.0.4.12"),
        ];
        assert_eq!(
            find(&hops, "ops@10.0.4.12's password: ").map(|h| h.name.as_str()),
            Some("lb-2")
        );
        // Another user on a known host, or an unknown host: nobody answers.
        assert!(find(&hops, "root@10.0.4.12's password: ").is_none());
        assert!(find(&hops, "ops@10.0.4.99's password: ").is_none());
        // A login-less hop answers for whichever user ssh defaulted to.
        let hops = [hop("lb-3", None, "Lb-3.example")];
        assert!(find(&hops, "anyone@lb-3.example's password: ").is_some());
    }

    #[test]
    fn two_hops_the_prompt_could_be_are_answered_by_neither() {
        let hops = [
            hop("a", Some("ops"), "10.0.4.11"),
            hop("b", Some("ops"), "10.0.4.11"),
        ];
        assert!(find(&hops, "ops@10.0.4.11's password: ").is_none());
    }

    #[test]
    fn a_hop_only_ever_answers_its_own_class() {
        let h = hop("lb-1", Some("ops"), "10.0.4.11");
        assert!(h.secret(PromptClass::Password).is_some());
        assert!(h.secret(PromptClass::Passphrase).is_none());
        assert!(h.secret(PromptClass::HostKey).is_none());
        let staged = HopSecret::of(&h);
        assert_eq!(staged.len(), 1);
        assert!(find_secret(&staged, "ops@10.0.4.11's password: ").is_some());
        assert!(find_secret(&staged, "ops@10.0.4.12's password: ").is_none());
        assert!(find_secret(&staged, "Enter passphrase for key '/k': ").is_none());
    }
}

use crate::matrix::{Budget, Rung, na, pending};

tear_types::closed_vocabulary! {
    DaemonPeer { Prev => "prev", Head => "head" }
}

tear_types::closed_vocabulary! {
    ClientPeer { Prev => "prev", Head => "head" }
}

tear_types::closed_vocabulary! {
    HolderPeer { Oldest => "oldest", Prev => "prev", Head => "head" }
}

tear_types::closed_vocabulary! {
    Check {
        Hello => "hello",
        Keys => "keys",
        Echo => "echo",
        AttachFrame => "attach-frame",
        Subscription => "subscription",
        Paste => "paste",
        Resize => "resize",
        Readopt => "readopt",
        Documents => "documents",
        Impose => "impose",
        MutedHolder => "muted-holder",
        StalledWindow => "stalled-window",
        Answers => "answers",
    }
}

pub const OLDEST_HOLDER: &str = "v0.1.28";

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Pairing {
    pub daemon: DaemonPeer,
    pub client: ClientPeer,
    pub holder: HolderPeer,
}

impl Pairing {
    #[must_use]
    pub fn all() -> Vec<Pairing> {
        let mut out = Vec::with_capacity(12);
        for daemon in DaemonPeer::ALL {
            for client in ClientPeer::ALL {
                for holder in HolderPeer::ALL {
                    out.push(Pairing {
                        daemon: *daemon,
                        client: *client,
                        holder: *holder,
                    });
                }
            }
        }
        out
    }

    #[must_use]
    pub fn name(self) -> String {
        format!(
            "daemon-{}.client-{}.holder-{}",
            self.daemon.name(),
            self.client.name(),
            self.holder.name()
        )
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompatCell {
    pub pairing: Pairing,
    pub check: Check,
}

impl CompatCell {
    #[must_use]
    pub const fn budget(self) -> Budget {
        budget(self.pairing, self.check)
    }

    #[must_use]
    pub fn case_name(self) -> String {
        format!("compat:{}", self.pairing.name())
    }

    #[must_use]
    pub fn name(self) -> String {
        format!("{}/{}", self.case_name(), self.check.name())
    }

    #[must_use]
    pub fn muted_holder(holder: HolderPeer) -> Self {
        Self {
            pairing: Pairing {
                daemon: DaemonPeer::Head,
                client: ClientPeer::Head,
                holder,
            },
            check: Check::MutedHolder,
        }
    }
}

#[must_use]
pub fn cells() -> Vec<CompatCell> {
    Pairing::all()
        .into_iter()
        .flat_map(|pairing| {
            Check::ALL.iter().map(move |check| CompatCell {
                pairing,
                check: *check,
            })
        })
        .collect()
}

#[must_use]
pub const fn budget(pairing: Pairing, check: Check) -> Budget {
    match check {
        Check::MutedHolder => muted_holder(pairing),
        Check::Hello
        | Check::Keys
        | Check::Echo
        | Check::AttachFrame
        | Check::Subscription
        | Check::Paste
        | Check::Resize
        | Check::Readopt
        | Check::Documents
        | Check::Impose
        | Check::StalledWindow
        | Check::Answers => pending(
            Rung::R15,
            "no compatibility cell runs this check yet",
            "§6 compatibility matrix, R15's gate",
        ),
    }
}

const fn muted_holder(pairing: Pairing) -> Budget {
    match (pairing.daemon, pairing.client) {
        (_, ClientPeer::Prev) => na(
            "the client takes no part in the holder link: the head-client cell of the same daemon and holder grades it",
        ),
        (DaemonPeer::Head, ClientPeer::Head) => Budget::Bytes { max: 0 },
        (DaemonPeer::Prev, ClientPeer::Head) => pending(
            Rung::R15,
            "a Prev daemon needs a Prev daemon in the harness; a Prev daemon before R4 over a pre-R4 holder stays muted until restart",
            "§7 rollback row; D",
        ),
    }
}

const fn check_compat() {
    let mut d = 0;
    while d < DaemonPeer::ALL.len() {
        let mut c = 0;
        while c < ClientPeer::ALL.len() {
            let mut h = 0;
            while h < HolderPeer::ALL.len() {
                let mut k = 0;
                while k < Check::ALL.len() {
                    crate::matrix::check_budget(budget(
                        Pairing {
                            daemon: DaemonPeer::ALL[d],
                            client: ClientPeer::ALL[c],
                            holder: HolderPeer::ALL[h],
                        },
                        Check::ALL[k],
                    ));
                    k += 1;
                }
                h += 1;
            }
            c += 1;
        }
        d += 1;
    }
}

const _: () = check_compat();

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn twelve_pairings_and_every_check_has_a_budget_or_a_reason() {
        assert_eq!(Pairing::all().len(), 12);
        assert_eq!(cells().len(), 12 * Check::ALL.len());
        let budgeted: Vec<CompatCell> = cells()
            .into_iter()
            .filter(|c| c.budget().is_budgeted())
            .collect();
        assert_eq!(
            budgeted,
            HolderPeer::ALL
                .iter()
                .map(|h| CompatCell::muted_holder(*h))
                .collect::<Vec<_>>()
        );
        for c in budgeted {
            assert_eq!(c.budget(), Budget::Bytes { max: 0 });
        }
    }
}

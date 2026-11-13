//! A scope is one (tenant, job): the unit the bus, the agents and the console are partitioned by.

use std::fmt;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Scope {
    pub tenant: Uuid,
    pub job: String,
}

impl Scope {
    /// `<uuid>/<job>`: a canonical (lowercase, hyphenated), non-nil tenant uuid and a job key matching
    /// `^[a-z][a-z0-9_]{0,62}$`. The inverse of `Display`, so one scope has exactly one registry member.
    pub fn parse(s: &str) -> Result<Scope> {
        let Some((tenant, job)) = s.split_once('/') else {
            bail!("scope {s:?} is not <tenant>/<job>");
        };
        let uuid =
            Uuid::parse_str(tenant).map_err(|e| anyhow::anyhow!("scope {s:?}: tenant: {e}"))?;
        if uuid.is_nil() {
            bail!("scope {s:?}: tenant is the nil uuid");
        }
        if uuid.hyphenated().to_string() != tenant {
            bail!("scope {s:?}: tenant is not a lowercase hyphenated uuid");
        }
        if !valid_job(job) {
            bail!("scope {s:?}: job must match ^[a-z][a-z0-9_]{{0,62}}$");
        }
        Ok(Scope {
            tenant: uuid,
            job: job.to_string(),
        })
    }
}

fn valid_job(job: &str) -> bool {
    let b = job.as_bytes();
    !b.is_empty()
        && b.len() <= 63
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.tenant.hyphenated(), self.job)
    }
}

#[cfg(test)]
mod tests {
    use super::Scope;

    const T: &str = "0195f48b-d000-7000-8000-000000000099";

    #[test]
    fn parses_and_round_trips() {
        let s = Scope::parse(&format!("{T}/customer_care_agent")).unwrap();
        assert_eq!(s.job, "customer_care_agent");
        assert_eq!(s.tenant.to_string(), T);
        assert_eq!(s.to_string(), format!("{T}/customer_care_agent"));
        assert_eq!(Scope::parse(&s.to_string()).unwrap(), s);
        let longest = format!("{T}/a{}", "b".repeat(62));
        assert!(Scope::parse(&longest).is_ok());
    }

    #[test]
    fn rejects_bad_scopes() {
        let too_long = format!("{T}/a{}", "b".repeat(63));
        for bad in [
            "00000000-0000-0000-0000-000000000000/x",
            "0195f48b-d000-7000-8000-000000000099/Bad",
            "0195f48b-d000-7000-8000-000000000099",
            "0195f48b-d000-7000-8000-000000000099/",
            "0195f48b-d000-7000-8000-000000000099/1job",
            "0195f48b-d000-7000-8000-000000000099/a-b",
            "0195F48B-D000-7000-8000-000000000099/x",
            "0195f48bd00070008000000000000099/x",
            "not-a-uuid/x",
            too_long.as_str(),
        ] {
            assert!(Scope::parse(bad).is_err(), "{bad}");
        }
    }
}

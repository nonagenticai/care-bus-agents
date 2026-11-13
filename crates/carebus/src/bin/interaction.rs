//! Interaction agent. `--daemon` (or CAREBUS_INTERACTION_DAEMON=1): consume request.resolved of every registered
//! scope forever and relay replies to web conversations. Default: `interaction <tenant>/<job>`, a chat REPL as the
//! customer of that one scope, for local use.
use std::sync::Arc;

use anyhow::Context;
use carebus::chat::{Mode, deliver_reply, mode_from};
use carebus::interaction::{Interaction, Qualifier, RigQualifier, RuleQualifier, Turn};
use carebus::spaces::{EnvSpaces, REFRESH, SpaceSource, run_with_catalog};
use carebus::supervisor::run_per_scope;
use carebus::{Bus, Scope};
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    carebus::init_tracing();
    let root = carebus::bus_from_env().await?;
    let args: Vec<String> = std::env::args().collect();
    let mode = mode_from(
        &args,
        std::env::var("CAREBUS_INTERACTION_DAEMON").ok().as_deref(),
    );
    if std::env::var("VLLM_LICENSE_TOKEN").is_ok() {
        run(root, &args, mode, RigQualifier::from_env, "rig extractor").await
    } else {
        run(
            root,
            &args,
            mode,
            || Ok(RuleQualifier),
            "keyword rules; set VLLM_LICENSE_TOKEN to use rig",
        )
        .await
    }
}

/// `make` builds one qualifier per interaction agent: one per scope in the daemon, one for the REPL.
async fn run<Q, M>(
    root: Bus,
    args: &[String],
    mode: Mode,
    make: M,
    qualifier: &str,
) -> anyhow::Result<()>
where
    Q: Qualifier,
    M: Fn() -> anyhow::Result<Q> + Send + Sync + 'static,
{
    tracing::info!(qualifier, "interaction starting");
    let consumer = carebus::consumer_name("interaction");
    // Each scope's context spaces (request types, customer streams per area): from carebus-api when CAREBUS_API_URL is set, else the bootstrap job's.
    let spaces = EnvSpaces::from_env()?;
    // Response gates (spec 2026-10-18 §3) through carebus-api: CAREBUS_API_URL + CAREBUS_API_TOKEN required unless
    // CAREBUS_GATES=off.
    let gates = carebus::gates::GateClient::from_env()?;
    match mode {
        // Daemon: one reply consumer per registered scope; the supervisor runs until the process ends.
        Mode::Daemon => {
            tracing::info!("interaction daemon: consuming request.resolved per scope");
            run_per_scope(root, "interaction", move |bus, s| {
                let agent =
                    make().map(|q| Interaction::new(bus.clone(), q).with_gates(gates.clone()));
                let src = spaces.for_scope(&s);
                let consumer = consumer.clone();
                async move {
                    match agent {
                        Ok(a) => {
                            run_with_catalog(&src, REFRESH, |spaces| {
                                Arc::new(a.with_spaces(spaces))
                                    .run_replies(&consumer, relay(bus, false))
                            })
                            .await
                        }
                        Err(e) => Err(e),
                    }
                }
            })
            .await
        }
        Mode::Repl => {
            let scope = repl_scope(args)?;
            let bus = root.scoped(&scope);
            let catalog = spaces.for_scope(&scope).catalog().await?;
            let agent = Arc::new(
                Interaction::new(bus.clone(), make()?)
                    .with_spaces(catalog)
                    .with_gates(gates),
            );
            println!("[qualifier: {qualifier}] [scope: {scope}]");
            let a = agent.clone();
            tokio::spawn(async move {
                if let Err(e) = a.run_replies(&consumer, relay(bus, true)).await {
                    eprintln!("reply consumer stopped: {e:#}");
                }
            });
            repl_loop(agent).await
        }
    }
}

/// The REPL's scope: the first argument that is not a flag.
fn repl_scope(args: &[String]) -> anyhow::Result<Scope> {
    let raw = args.iter().skip(1).find(|a| !a.starts_with("--")).context(
        "usage: interaction <tenant>/<job> (REPL as that scope's customer) | interaction --daemon",
    )?;
    Scope::parse(raw)
}

/// Relay each composed reply to the web conversation linked to its case (REPL cases have none).
fn relay(bus: Bus, repl: bool) -> impl Fn(String, String) + Send + Sync + 'static {
    move |case: String, text: String| {
        if repl {
            println!("\nagent ({case}): {text}\n> ");
        }
        let bus = bus.clone();
        tokio::spawn(async move {
            match deliver_reply(&bus, &case, &text).await {
                Ok(true) => {}
                Ok(false) => tracing::debug!(case, "no web conversation for this case"),
                Err(e) => tracing::error!(case, "relaying reply failed: {e:#}"),
            }
        });
    }
}

async fn repl_loop<Q: Qualifier>(agent: Arc<Interaction<Q>>) -> anyhow::Result<()> {
    println!("agent: Hi Anna, how can I help?");
    let mut draft = None;
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    print!("> ");
    while let Some(line) = lines.next_line().await? {
        match agent.handle(&mut draft, &line).await? {
            Turn::Ask(q) | Turn::Unclear(q) | Turn::Gated(q) => println!("agent: {q}"),
            Turn::Published(case) => {
                println!("agent: Thanks, I have everything I need. Checking now. [{case}]")
            }
        }
        print!("> ");
    }
    Ok(())
}

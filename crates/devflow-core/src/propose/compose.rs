//! Compose services devflow does not branch: started per workspace through
//! hooks, isolated by `COMPOSE_PROJECT_NAME` where Docker lets a project name
//! isolate them. A service devflow branches itself (PostgreSQL, Valkey, Redis,
//! MySQL 8.4: see `containers`) is left out, and so is one the configuration
//! already has.
//!
//! A service devflow only *proposes* to branch (`service:<key>` is an item the
//! person can decline) is a choice between two ways to run it: devflow's own
//! container when the item is accepted, the compose start when it is not. The
//! choice is made at selection. The hook items carry what they need to be
//! rendered again (`value.compose`: the services in compose order, the item
//! that branches each, their risks and the patterns of the title and the
//! command), and [`Proposal::resolved`](super::Proposal::resolved) renders them
//! from the final set of accepted ids; the plan shows the hook as it reads
//! when every proposed service is accepted.
//!
//! A project name prefixes containers, networks and volumes, but not a
//! top-level volume with an explicit `name:` or an `external` one: every
//! checkout that declares it uses the same Docker volume. The teardown hook
//! therefore deletes volumes (`down -v`) only when every volume definition was
//! read and none is shared; otherwise it is a plain `down`.
use super::hooks::Purpose;
use super::plan::{Builder, scrub};
use super::stack::Step;
use super::{Confidence, Item, ItemId, PatchOp};
use crate::detect::{ComposeService, ComposeVolume, Engine, shell_quote};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How a compose hook item is made, stored in its `value.compose` so that it
/// can be rendered again for the services that end up branched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct ComposeHook {
    /// The services the project's compose file could start, in its order.
    members: Vec<Member>,
    /// The title; `{}` is the list of the services the hook starts.
    title: String,
    /// The command; `{}` is the quoted names. `None`: the command names no
    /// service (a `down`), and the item's `run` stays as it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    run: Option<String>,
    /// The first risk; `{}` is the list.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    lead: Option<String>,
    /// Risks that follow the per-service ones.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tail: Vec<String>,
    /// Evidence that does not depend on the services (the compose file).
    evidence: Vec<String>,
}

/// One compose service of a [`ComposeHook`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Member {
    service: String,
    /// The service item that makes devflow run this service itself. While it
    /// is not accepted the hook starts the service. `None`: devflow does not
    /// branch it, the hook always starts it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branched_by: Option<ItemId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    evidence: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    risks: Vec<String>,
}

/// A [`ComposeHook`] for one choice of services.
pub(super) struct Rendered {
    pub title: String,
    pub run: Option<String>,
    pub risks: Vec<String>,
    pub evidence: Vec<String>,
}

impl ComposeHook {
    /// The hook as the plan shows it: it starts what devflow never branches,
    /// and, when that is nothing, everything it could start (the alternative
    /// to branching).
    pub fn rendered_for_plan(&self) -> Option<Rendered> {
        let always = self.members.iter().any(|m| m.branched_by.is_none());
        self.render(&|m| !always || m.branched_by.is_none(), true)
    }

    /// The hook for the `accepted` ids: a service whose branching item is
    /// accepted is devflow's, every other one is started here.
    pub fn rendered_for(&self, accepted: &[ItemId]) -> Option<Rendered> {
        self.render(
            &|m| {
                !m.branched_by
                    .as_ref()
                    .is_some_and(|id| accepted.contains(id))
            },
            false,
        )
    }

    /// `None`: no service is left to start.
    fn render(&self, started: &dyn Fn(&Member) -> bool, pointers: bool) -> Option<Rendered> {
        let kept: Vec<&Member> = self.members.iter().filter(|m| started(m)).collect();
        if kept.is_empty() {
            return None;
        }
        let list = kept
            .iter()
            .map(|m| m.service.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let quoted = kept
            .iter()
            .map(|m| shell_quote(&m.service))
            .collect::<Vec<_>>()
            .join(" ");
        let mut risks: Vec<String> = self
            .lead
            .iter()
            .map(|lead| lead.replace("{}", &list))
            .collect();
        let mut evidence = self.evidence.clone();
        for member in &kept {
            risks.extend(member.risks.clone());
            for line in &member.evidence {
                if !evidence.contains(line) {
                    evidence.push(line.clone());
                }
            }
        }
        risks.extend(self.tail.clone());
        if pointers {
            for member in self.members.iter().filter(|m| !started(m)) {
                if let Some(item) = &member.branched_by {
                    risks.push(format!(
                        "`{}` is not started here while `{item}` is accepted: devflow runs its \
                         own container then. If you decline `{item}`, this hook starts `{}` \
                         too.",
                        member.service, member.service
                    ));
                }
            }
        }
        Some(Rendered {
            title: self.title.replace("{}", &list),
            run: self.run.as_ref().map(|run| run.replace("{}", &quoted)),
            risks,
            evidence,
        })
    }
}

impl Builder<'_> {
    pub fn compose(&mut self) {
        let facts = self.facts;
        let Some(compose) = &facts.compose else {
            return;
        };
        // The services the hooks could start, in compose order, with the
        // item that makes devflow run them itself, when it is only proposed.
        let mut members: Vec<(&ComposeService, Option<ItemId>)> = Vec::new();
        let mut unrecognized = Vec::new();
        let mut built = Vec::new();
        let mut profiled = Vec::new();
        for service in &compose.services {
            if !valid_service_name(&service.name) {
                // A name like `--build` would become an option of the command.
                self.warn(format!(
                    "{}: the service name {:?} is not one Docker Compose accepts; it is left \
                     out of the proposed hooks",
                    compose.file, service.name
                ));
            } else if self.branched_compose.contains(&service.name) {
                // devflow's own container replaces it, unless the item that
                // adds it is declined: then the compose start is the way.
                if let Some(item) = self.branch_fallback.get(&service.name)
                    && !service.build
                    && service.profiles.is_empty()
                {
                    members.push((service, Some(item.clone())));
                }
            } else if service.build {
                built.push(service.name.clone());
            } else if service.engine == Engine::Other {
                unrecognized.push(format!(
                    "`{}`{}",
                    service.name,
                    service
                        .image
                        .as_deref()
                        .map(|i| format!(" ({i})"))
                        .unwrap_or_default()
                ));
            } else if service.engine == Engine::Sqlite {
                // A file, never a container.
            } else if self.has_pg && service.engine == Engine::Postgres {
                // devflow's server replaces it.
            } else if !service.profiles.is_empty() {
                profiled.push(service.name.clone());
            } else {
                members.push((service, None));
            }
        }
        if !built.is_empty() {
            self.warn(format!(
                "{}: services built from source ({}) are not started by the proposed compose \
                 hooks",
                compose.file,
                built.join(", ")
            ));
        }
        if !unrecognized.is_empty() {
            self.warn(format!(
                "{}: {} {} not a datastore devflow recognizes and {} left out of the proposed \
                 compose hooks",
                compose.file,
                unrecognized.join(", "),
                if unrecognized.len() == 1 { "is" } else { "are" },
                if unrecognized.len() == 1 { "is" } else { "are" },
            ));
        }
        if !profiled.is_empty() {
            self.warn(format!(
                "{}: services behind compose profiles ({}) are left out of the proposed hooks",
                compose.file,
                profiled.join(", ")
            ));
        }
        if members.is_empty() {
            return;
        }
        self.compose_hooks = true;

        let evidence = vec![compose.file.clone()];

        // Volumes the project name cannot isolate.
        let shared: Vec<&ComposeVolume> = compose.volumes.iter().filter(|v| v.shared()).collect();
        let isolated = shared.is_empty() && compose.volumes_complete;

        let lead = if isolated {
            "devflow does not branch {} (it branches PostgreSQL, Valkey, Redis and MySQL 8.4): \
             every workspace starts with empty data in its own containers (COMPOSE_PROJECT_NAME \
             keeps them apart)."
                .to_string()
        } else {
            "devflow does not branch {} (it branches PostgreSQL, Valkey, Redis and MySQL 8.4). \
             COMPOSE_PROJECT_NAME keeps the workspaces' containers apart, but not every volume: \
             see the teardown hook."
                .to_string()
        };
        let mut setup_members = Vec::new();
        let mut plain_members = Vec::new();
        for (service, branched_by) in &members {
            let service = *service;
            let mut risks = Vec::new();
            if let Some(why) = self.unbranched_compose.get(&service.name) {
                risks.push(format!("`{}` is not branched: {why}.", service.name));
            }
            if let Some(name) = &service.container_name {
                risks.push(format!(
                    "`{}` sets container_name: {name}. A fixed container name collides between \
                     workspaces and COMPOSE_PROJECT_NAME cannot rename it; remove it.",
                    service.name
                ));
            }
            for port in &service.ports {
                if let Some(host) = port.host {
                    risks.push(format!(
                        "`{}` publishes fixed host port {host}. Two workspaces cannot both run \
                         it; publish an ephemeral port (`\"{}\"`) or take the port from a \
                         variable.",
                        service.name, port.container
                    ));
                }
            }
            for volume in &service.volumes {
                if let Some(source) = &volume.source
                    && !volume.bind
                {
                    let top_level = compose.volumes.iter().find(|v| v.name == *source);
                    risks.push(match top_level {
                        Some(shared) if shared.shared() => format!(
                            "`{}` keeps data in the volume `{source}`, which {}: the main \
                             checkout and every workspace use the same data, whatever \
                             COMPOSE_PROJECT_NAME is.",
                            service.name,
                            sharing(shared)
                        ),
                        _ if isolated => format!(
                            "`{}` keeps data in the named volume `{source}`; each workspace gets \
                             its own empty copy, and `docker compose down -v` in teardown deletes \
                             it.",
                            service.name
                        ),
                        _ => format!(
                            "`{}` keeps data in the named volume `{source}`; devflow could not \
                             tell whether each workspace gets its own copy.",
                            service.name
                        ),
                    });
                }
            }
            risks.iter_mut().for_each(scrub);
            plain_members.push(Member {
                service: service.name.clone(),
                branched_by: branched_by.clone(),
                evidence: service.evidence.clone(),
                risks: Vec::new(),
            });
            setup_members.push(Member {
                risks,
                ..plain_members[plain_members.len() - 1].clone()
            });
        }

        let up = Some("docker compose up -d --wait {}".to_string());
        self.compose_hook(
            "setup",
            ("hook:setup:compose-up", "compose-up"),
            "",
            ComposeHook {
                members: setup_members,
                title: "Start {} with Docker Compose in each new workspace".into(),
                run: up.clone(),
                lead: Some(lead),
                tail: Vec::new(),
                evidence: evidence.clone(),
            },
        );
        self.compose_hook(
            "switch",
            ("hook:switch:compose-up", "compose-up"),
            "",
            ComposeHook {
                members: plain_members.clone(),
                title: "Start {} again when switching to a workspace (after a restart)".into(),
                run: up,
                lead: None,
                tail: vec![
                    "Runs on every switch into a ready workspace; `up -d --wait` does \
                     nothing when the containers are already running."
                        .into(),
                ],
                evidence: evidence.clone(),
            },
        );
        if isolated {
            self.compose_hook(
                "teardown",
                ("hook:teardown:compose-down", "compose-down"),
                "docker compose down -v --remove-orphans || true",
                ComposeHook {
                    members: plain_members,
                    title: "Stop {} and delete their volumes when the workspace is removed".into(),
                    run: None,
                    lead: None,
                    tail: vec![
                        "`-v` deletes the workspace's compose volumes. They belong to this \
                         workspace's COMPOSE_PROJECT_NAME, never to the main checkout's project."
                            .into(),
                        "`|| true` keeps a stopped Docker daemon from blocking the removal of a workspace."
                            .into(),
                    ],
                    evidence,
                },
            );
        } else {
            let mut tail = Vec::new();
            if !shared.is_empty() {
                let described: Vec<String> = shared
                    .iter()
                    .map(|v| format!("`{}` ({})", v.name, sharing(v)))
                    .collect();
                tail.push(format!(
                    "Not isolated: {} {} the same Docker volume in every checkout, whatever \
                     COMPOSE_PROJECT_NAME is, so the main checkout's data is in it too. `down -v` \
                     would delete it; the teardown only stops the containers and keeps every \
                     volume. Give the volume no `name:` (or declare it per project) for an \
                     isolated copy, and delete it by hand when you no longer need the data.",
                    described.join(", "),
                    if described.len() == 1 { "is" } else { "are" },
                ));
            }
            if !compose.volumes_complete {
                tail.push(
                    "devflow could not read every volume definition (an include, an override \
                     file or a definition that is not plain data), so it cannot tell which \
                     volumes are shared with the main checkout. The teardown does not use `-v`: \
                     remove the workspace's volumes yourself if you want them gone."
                        .to_string(),
                );
            }
            tail.push(
                "`|| true` keeps a stopped Docker daemon from blocking the removal of a workspace."
                    .into(),
            );
            self.compose_hook(
                "teardown",
                ("hook:teardown:compose-down", "compose-down"),
                "docker compose down --remove-orphans || true",
                ComposeHook {
                    members: plain_members,
                    title: "Stop {} when the workspace is removed (volumes are kept)".into(),
                    run: None,
                    lead: None,
                    tail,
                    evidence,
                },
            );
        }
    }

    /// Propose one compose hook, rendered for the plan, with what a selection
    /// needs to render it again in its `value`.
    fn compose_hook(
        &mut self,
        phase: &'static str,
        (id, name): (&str, &str),
        fixed_run: &str,
        spec: ComposeHook,
    ) {
        let Some(rendered) = spec.rendered_for_plan() else {
            return;
        };
        let purpose = if phase == "teardown" {
            Purpose::ComposeDown
        } else {
            Purpose::ComposeUp
        };
        let step = Step {
            id: id.into(),
            name: name.into(),
            run: rendered.run.unwrap_or_else(|| fixed_run.to_string()),
            title: rendered.title,
            confidence: Confidence::Medium,
            evidence: rendered.evidence,
            risks: rendered.risks,
            purpose,
            depends_on: Vec::new(),
        };
        self.hook_with(phase, step, Some(&spec));
    }
}

/// `item` as it reads for the `accepted` ids. A compose hook is rendered for
/// the services devflow does not run itself; one with nothing left to start
/// is dropped. Every other item is returned as it is.
pub(super) fn resolve(mut item: Item, accepted: &[ItemId]) -> Option<Item> {
    if !accepted.contains(&item.id) {
        return Some(item);
    }
    let Some(hook) = item
        .value
        .get("compose")
        .and_then(|raw| serde_json::from_value::<ComposeHook>(raw.clone()).ok())
    else {
        return Some(item);
    };
    let rendered = hook.rendered_for(accepted)?;
    item.title = rendered.title;
    item.risks = rendered.risks;
    item.evidence = rendered.evidence;
    if let Some(run) = rendered.run {
        let name = item.value.get("name").cloned();
        for op in &mut item.patch.ops {
            if let PatchOp::Append { values, .. } = op {
                for value in values.iter_mut().filter(|v| v.get("name") == name.as_ref()) {
                    value["run"] = Value::String(run.clone());
                }
            }
        }
        item.value["run"] = Value::String(run);
    }
    Some(item)
}

/// Why a volume is shared, for messages.
fn sharing(volume: &ComposeVolume) -> &'static str {
    match (volume.external, volume.explicit_name) {
        (true, true) => "is external and has an explicit name",
        (true, false) => "is external",
        _ => "has an explicit name",
    }
}

/// Docker Compose's own rule for service names: it starts with a letter or
/// digit, so a name can never be read as an option.
pub(super) fn valid_service_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

#[cfg(test)]
mod tests {
    use super::valid_service_name;

    #[test]
    fn service_names_cannot_become_options() {
        for ok in ["db", "redis-1", "Cache_2", "a.b"] {
            assert!(valid_service_name(ok), "{ok}");
        }
        for bad in ["", "--build", "-d", "a b", "a;b", "$(id)", "é"] {
            assert!(!valid_service_name(bad), "{bad}");
        }
    }
}

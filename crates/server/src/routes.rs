//! The route list (`spec/api.md`, "Routes"), as `GET /v1` sends it. A test checks every route and
//! parameter here against `spec/api.md`.

use serde::Serialize;

/// Who may call a route (`spec/api.md`, "Access").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Read,
    Client,
    Producer,
    Local,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Param {
    pub name: &'static str,
    #[serde(rename = "in")]
    pub place: &'static str,
    pub summary: &'static str,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Route {
    pub method: &'static str,
    pub path: &'static str,
    pub summary: &'static str,
    pub access: Access,
    pub params: &'static [Param],
}

const fn p(name: &'static str, place: &'static str, summary: &'static str) -> Param {
    Param { name, place, summary }
}

const SOURCE: Param = p("source", "path", "A source name.");
const ID: Param = p("id", "path", "An id Beacon assigned.");
const IDEMPOTENCY: Param = p(
    "Idempotency-Key",
    "header",
    "1 to 128 printable ASCII characters: the same key and body within a day answers the first answer again.",
);

pub static ROUTES: &[Route] = &[
    Route {
        method: "GET",
        path: "/v1",
        summary: "This list.",
        access: Access::Read,
        params: &[],
    },
    Route {
        method: "GET",
        path: "/v1/health",
        summary: "Whether Beacon is up, and whether it can push.",
        access: Access::Read,
        params: &[],
    },
    Route {
        method: "GET",
        path: "/v1/registry",
        summary: "Every source: name, icon, kind count, whether it is observed.",
        access: Access::Read,
        params: &[],
    },
    Route {
        method: "GET",
        path: "/v1/registry/{source}",
        summary: "One source's declarations.",
        access: Access::Read,
        params: &[SOURCE],
    },
    Route {
        method: "PUT",
        path: "/v1/registry/{source}",
        summary: "Publish a source's declarations.",
        access: Access::Producer,
        params: &[SOURCE],
    },
    Route {
        method: "DELETE",
        path: "/v1/registry/{source}",
        summary: "Remove a source's declarations.",
        access: Access::Producer,
        params: &[SOURCE],
    },
    Route {
        method: "GET",
        path: "/v1/kinds",
        summary: "Every kind, flat, with its source's name and icon: what the apps register notification categories from.",
        access: Access::Read,
        params: &[p(
            "If-None-Match",
            "header",
            "The ETag of an earlier answer: 304 when nothing changed.",
        )],
    },
    Route {
        method: "POST",
        path: "/v1/events",
        summary: "One notification: open, update or resolve an alert or action, or record an update or record.",
        access: Access::Producer,
        params: &[IDEMPOTENCY],
    },
    Route {
        method: "POST",
        path: "/v1/snapshots",
        summary: "A source's complete set of open alerts and actions; everything absent resolves.",
        access: Access::Producer,
        params: &[],
    },
    Route {
        method: "POST",
        path: "/v1/trail",
        summary: "Trail's alert webhook: its snapshot and notice bodies.",
        access: Access::Local,
        params: &[],
    },
    Route {
        method: "POST",
        path: "/v1/presence",
        summary: "A client's heartbeat: active or not, and what it is viewing.",
        access: Access::Client,
        params: &[],
    },
    Route {
        method: "GET",
        path: "/v1/inbox",
        summary: "Needs you, alerts, activity, held items, the dot, and since your last visit.",
        access: Access::Read,
        params: &[
            p(
                "source",
                "query",
                "Activity only: a source, or several separated by commas.",
            ),
            p(
                "class",
                "query",
                "Activity only: classes separated by commas. Default update.",
            ),
            p(
                "before",
                "query",
                "Activity only: the previous page's activity.next.",
            ),
            p("limit", "query", "Activity page size, 1 to 200. Default 50."),
            p("since", "query", "held only: a version from an earlier answer."),
            p(
                "If-None-Match",
                "header",
                "The ETag of an earlier answer to the same URL: 304 when nothing changed.",
            ),
        ],
    },
    Route {
        method: "GET",
        path: "/v1/items/{id}",
        summary: "One item, with its facts, slots, history and push attempts.",
        access: Access::Read,
        params: &[ID],
    },
    Route {
        method: "POST",
        path: "/v1/items/{id}/actions",
        summary: "Press a button: done, acknowledge, snooze, dismiss, silence_1h or a custom action.",
        access: Access::Client,
        params: &[ID],
    },
    Route {
        method: "POST",
        path: "/v1/seen",
        summary: "Mark items seen.",
        access: Access::Client,
        params: &[],
    },
    Route {
        method: "GET",
        path: "/v1/silences",
        summary: "Active silences, or all recent ones.",
        access: Access::Read,
        params: &[p(
            "all",
            "query",
            "1 adds the silences that ended in the last 7 days.",
        )],
    },
    Route {
        method: "POST",
        path: "/v1/silences",
        summary: "Open a silence.",
        access: Access::Client,
        params: &[IDEMPOTENCY],
    },
    Route {
        method: "GET",
        path: "/v1/silences/{id}",
        summary: "One silence.",
        access: Access::Read,
        params: &[ID],
    },
    Route {
        method: "PATCH",
        path: "/v1/silences/{id}",
        summary: "Move a silence's end, or change its reason.",
        access: Access::Client,
        params: &[ID],
    },
    Route {
        method: "DELETE",
        path: "/v1/silences/{id}",
        summary: "End a silence now.",
        access: Access::Client,
        params: &[ID],
    },
    Route {
        method: "GET",
        path: "/v1/devices",
        summary: "Registered iPhones, with their tokens cut short.",
        access: Access::Read,
        params: &[],
    },
    Route {
        method: "POST",
        path: "/v1/devices",
        summary: "Register an APNs device token.",
        access: Access::Client,
        params: &[],
    },
    Route {
        method: "DELETE",
        path: "/v1/devices/{token}",
        summary: "Forget a device.",
        access: Access::Client,
        params: &[p("token", "path", "The device's whole APNs token.")],
    },
    Route {
        method: "POST",
        path: "/v1/live-activity",
        summary: "Send one Live Activity update or end straight to Apple.",
        access: Access::Local,
        params: &[],
    },
];

/// The route a path names, with its `{...}` parts. Matches by path alone; the caller checks the
/// method.
pub fn matches(pattern: &str, path: &str) -> Option<Vec<String>> {
    let mut found = Vec::new();
    let mut want = pattern.split('/');
    let mut have = path.split('/');
    loop {
        match (want.next(), have.next()) {
            (None, None) => return Some(found),
            (Some(w), Some(h)) if w.starts_with('{') && !h.is_empty() => found.push(h.to_string()),
            (Some(w), Some(h)) if w == h => {}
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SPEC: &str = include_str!("../../../spec/api.md");

    fn access(a: Access) -> &'static str {
        match a {
            Access::Read => "read",
            Access::Client => "client",
            Access::Producer => "producer",
            Access::Local => "local",
        }
    }

    /// Every row of the spec's route table is a route here, with the same access and summary,
    /// and the other way round.
    #[test]
    fn the_routes_are_the_specs() {
        let rows: Vec<(String, String, String, String)> = SPEC
            .lines()
            .filter(|line| {
                line.starts_with("| `GET`") || line.starts_with("| `P") || line.starts_with("| `DELETE`")
            })
            .map(|line| {
                let cells: Vec<String> = line
                    .trim_matches('|')
                    .split(" | ")
                    .map(|c| c.trim().trim_matches('`').to_string())
                    .collect();
                (
                    cells[0].clone(),
                    cells[1].clone(),
                    cells[2].clone(),
                    cells[3].clone(),
                )
            })
            .collect();
        assert_eq!(rows.len(), ROUTES.len(), "{rows:?}");
        for route in ROUTES {
            let row = rows
                .iter()
                .find(|(m, p, _, _)| m == route.method && p == route.path)
                .unwrap_or_else(|| panic!("{} {} is not in spec/api.md", route.method, route.path));
            assert_eq!(row.2, access(route.access), "{} {}", route.method, route.path);
            assert_eq!(row.3, route.summary, "{} {}", route.method, route.path);
        }
    }

    /// Every parameter is named in spec/api.md.
    #[test]
    fn every_parameter_is_in_the_spec() {
        for route in ROUTES {
            for param in route.params {
                let named = format!("`{}`", param.name);
                let braced = format!("{{{}}}", param.name);
                let bold = format!("**{}**", param.name);
                let query = format!("?{}=", param.name);
                assert!(
                    [named, braced, bold, query]
                        .iter()
                        .any(|form| SPEC.contains(form.as_str())),
                    "{} of {} {} is not in spec/api.md",
                    param.name,
                    route.method,
                    route.path
                );
            }
        }
    }

    #[test]
    fn paths_match_with_their_parts() {
        assert_eq!(
            matches("/v1/items/{id}/actions", "/v1/items/4/actions"),
            Some(vec!["4".into()])
        );
        assert_eq!(matches("/v1/items/{id}", "/v1/items/"), None);
        assert_eq!(matches("/v1", "/v1"), Some(vec![]));
        assert_eq!(matches("/v1/inbox", "/v1/inbox/x"), None);
    }
}

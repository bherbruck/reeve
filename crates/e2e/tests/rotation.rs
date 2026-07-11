//! E1 secrets rotation bounces only consuming services (docs/build-
//! charter.md; spec/reeve/10-secrets.md §12.4): a rotation bumps the
//! consuming app's `secrets_version` and its manifestVersion — with the
//! bundle digest UNCHANGED — so the agent does a minimal re-`up` of
//! exactly the referencing app, and leaves every other app alone. The
//! fake provider records every `up -d`; the assertion is on WHICH app
//! re-upped. Extension-gated (ext-secrets on both server and agent);
//! compiled out of the conformance (core) build.
#![cfg(feature = "ext")]

use e2e::{Author, FakeProvider, TestAgent, boot, enroll_device};
use reeve_agent::PollOutcome;

const WEB_MANIFEST: &str = "\
apiVersion: margo.org/v1-alpha1
kind: ApplicationDescription
metadata:
  id: web
  name: Web
  version: 1.0.0
  catalog:
    organization:
      - name: Reeve Tests
        site: https://example.com
deploymentProfiles:
  - type: compose
    id: web-compose
    components:
      - name: web-stack
        properties:
          packageLocation: ./compose.yml
parameters:
  greeting:
    value: hello
    targets:
      - pointer: ENV.GREETING
        components: [\"web-stack\"]
";

const OTHER_MANIFEST: &str = "\
apiVersion: margo.org/v1-alpha1
kind: ApplicationDescription
metadata:
  id: other
  name: Other
  version: 1.0.0
  catalog:
    organization:
      - name: Reeve Tests
        site: https://example.com
deploymentProfiles:
  - type: compose
    id: other-compose
    components:
      - name: other-stack
        properties:
          packageLocation: ./compose.yml
";

const COMPOSE: &str = "\
services:
  app:
    image: ${REEVE_REGISTRY}/nginx:1.25
";

#[tokio::test]
async fn rotation_reups_only_the_consuming_app() {
    let srv = boot().await;
    let author = Author::new(&srv.base());
    let token = enroll_device(&srv.state, "dev-1", None);

    // A fleet secret and two apps: `web` REFERENCES it (via its greeting
    // param), `other` does not.
    author.put_secret("db-password", "fleet", "v1").await;
    author.put_package("web", "1.0.0", &[("margo.yaml", WEB_MANIFEST), ("compose.yml", COMPOSE)]).await;
    author.put_package("other", "1.0.0", &[("margo.yaml", OTHER_MANIFEST), ("compose.yml", COMPOSE)]).await;
    author
        .put_layer(
            "00-fleet",
            &[
                ("apps/web/app.yaml", "package:\n  name: web\n  version: 1.0.0\n"),
                ("apps/other/app.yaml", "package:\n  name: other\n  version: 1.0.0\n"),
                ("apps/web/params.yaml", "greeting: \"${secret:db-password}\"\n"),
            ],
        )
        .await;

    let provider = FakeProvider::new();
    let mut agent = TestAgent::http(&srv.base(), "dev-1", &token);
    agent.recover();

    // First converge brings BOTH apps up.
    let first = agent.tick(&provider).await;
    assert!(matches!(first.poll, PollOutcome::Accepted { .. }));
    let mut acted = first.acted.clone();
    acted.sort();
    assert_eq!(acted, ["other", "web"], "both apps converged first pass");
    assert_eq!(provider.up_count("web"), 1);
    assert_eq!(provider.up_count("other"), 1);

    // Capture the bundle digest — rotation MUST NOT change it (§12.4:
    // no rendered byte moves, only the resolved secret version).
    let digest_before = agent.store.current_digest();

    // Rotate the secret: same (name, scope), new value => version 2.
    let v = author.put_secret("db-password", "fleet", "v2").await;
    assert_eq!(v, 2, "rotation bumps the secret version");

    // The agent polls: a secrets-only manifest change (new
    // manifestVersion, same bundle digest). Converge re-ups ONLY web.
    let second = agent.tick(&provider).await;
    assert!(
        matches!(second.poll, PollOutcome::Accepted { .. }),
        "rotation bumps manifestVersion, got {:?}",
        second.poll
    );
    assert_eq!(second.acted, ["web"], "only the secret-consuming app re-upped");
    assert_eq!(provider.up_count("web"), 2, "web bounced");
    assert_eq!(provider.up_count("other"), 1, "other untouched");
    assert_eq!(
        agent.store.current_digest(),
        digest_before,
        "bundle digest unchanged across rotation — no re-pull, a minimal re-up"
    );
}

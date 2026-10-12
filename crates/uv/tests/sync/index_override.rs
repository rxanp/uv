//! Integration tests for installing locked distributions from indexes provided on the command line.

use anyhow::Result;
use assert_fs::prelude::*;
use indoc::formatdoc;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use uv_static::EnvVars;
use uv_test::package_server::PackageServer;
use uv_test::{TestContext, uv_snapshot};

/// Write a project that depends on `basic-package`, locked to download the wheel `filename` from
/// `origin`.
fn write_basic_package_lockfile(
    context: &TestContext,
    origin: &MockServer,
    filename: &str,
) -> Result<()> {
    context
        .temp_dir
        .child("pyproject.toml")
        .write_str(&formatdoc! { r#"
        [project]
        name = "project"
        version = "0.1.0"
        requires-python = ">=3.13"
        dependencies = ["basic-package"]

        [tool.uv.sources]
        basic-package = {{ index = "test-registry" }}

        [[tool.uv.index]]
        name = "test-registry"
        url = "{}/simple"
        "#,
            origin.uri()
        })?;

    context.temp_dir.child("uv.lock").write_str(&formatdoc! {r#"
        version = 1
        revision = 3
        requires-python = ">=3.13"

        [[package]]
        name = "basic-package"
        version = "0.1.0"
        source = {{ registry = "{origin}/simple" }}
        wheels = [
            {{ url = "{origin}/files/{filename}", hash = "sha256:7b6229db79b5800e4e98a351b5628c1c8a944533a2d428aeeaa7275a30d4ea82", size = 1548 }},
        ]

        [[package]]
        name = "project"
        version = "0.1.0"
        source = {{ virtual = "." }}
        dependencies = [
            {{ name = "basic-package" }},
        ]

        [package.metadata]
        requires-dist = [{{ name = "basic-package", index = "{origin}/simple" }}]
        "#, origin = origin.uri()})?;

    Ok(())
}

/// Download locked files from an index provided via `UV_INDEX` instead of the lockfile URL.
#[tokio::test]
async fn sync_frozen_index_override() -> Result<()> {
    let context = uv_test::test_context!("3.13");
    let wheel = fs_err::read(
        context
            .workspace_root
            .join("test/links/basic_package-0.1.0-py3-none-any.whl"),
    )?;

    // The lockfile URL must not be used when the file is available on the override index.
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&origin)
        .await;
    write_basic_package_lockfile(&context, &origin, "basic_package-0.1.0-py3-none-any.whl")?;

    let mirror = PackageServer::new(&"basic-package".parse()?).await;
    mirror
        .serve(
            "basic_package-0.1.0-py3-none-any.whl",
            &wheel,
            Some("7b6229db79b5800e4e98a351b5628c1c8a944533a2d428aeeaa7275a30d4ea82"),
        )
        .await;

    uv_snapshot!(context.filters(), context.sync().arg("--frozen").env(EnvVars::UV_INDEX, mirror.index_url()), @"
    exit_code: 0 (success)
    ----- stderr -----
    Prepared 1 package in [TIME]
    Installed 1 package in [TIME]
     + basic-package==0.1.0
    ");

    Ok(())
}

/// Download locked files that are missing from the override index from the lockfile URL.
#[tokio::test]
async fn sync_frozen_index_override_fallback() -> Result<()> {
    let context = uv_test::test_context!("3.13");
    let wheel = fs_err::read(
        context
            .workspace_root
            .join("test/links/basic_package-0.1.0-py3-none-any.whl"),
    )?;

    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/basic_package-0.1.0-py3-none-any.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel))
        .expect(1)
        .mount(&origin)
        .await;
    write_basic_package_lockfile(&context, &origin, "basic_package-0.1.0-py3-none-any.whl")?;

    // The override index doesn't have the package.
    let mirror = MockServer::start().await;

    uv_snapshot!(context.filters(), context.sync().arg("--frozen").arg("--index").arg(format!("{}/simple", mirror.uri())), @"
    exit_code: 0 (success)
    ----- stderr -----
    Prepared 1 package in [TIME]
    Installed 1 package in [TIME]
     + basic-package==0.1.0
    ");

    Ok(())
}

/// Use the lockfile URL, with a warning, when the override index advertises a different hash.
#[tokio::test]
async fn sync_frozen_index_override_advertised_hash_mismatch() -> Result<()> {
    let context = uv_test::test_context!("3.13");
    let wheel = fs_err::read(
        context
            .workspace_root
            .join("test/links/basic_package-0.1.0-py3-none-any.whl"),
    )?;

    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/files/basic_package-0.1.0-py3-none-any.whl"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(wheel.clone()))
        .expect(1)
        .mount(&origin)
        .await;
    write_basic_package_lockfile(&context, &origin, "basic_package-0.1.0-py3-none-any.whl")?;

    let mirror = PackageServer::new(&"basic-package".parse()?).await;
    mirror
        .serve(
            "basic_package-0.1.0-py3-none-any.whl",
            &wheel,
            Some(&"0".repeat(64)),
        )
        .await;

    uv_snapshot!(context.filters(), context.sync().arg("--frozen").arg("--index").arg(mirror.index_url()), @"
    exit_code: 0 (success)
    ----- stderr -----
    warning: Ignoring `basic_package-0.1.0-py3-none-any.whl` on http://[LOCALHOST]/simple because its hash does not match the lockfile
    Prepared 1 package in [TIME]
    Installed 1 package in [TIME]
     + basic-package==0.1.0
    ");

    Ok(())
}

/// Verify files from the override index against the hashes in the lockfile.
#[tokio::test]
async fn sync_frozen_index_override_hash_verification() -> Result<()> {
    let context = uv_test::test_context!("3.13");

    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&origin)
        .await;
    write_basic_package_lockfile(&context, &origin, "basic_package-0.1.0-py3-none-any.whl")?;

    // The override index serves different contents under the locked filename, without
    // advertising a hash.
    let mirror = PackageServer::new(&"basic-package".parse()?).await;
    mirror
        .serve(
            "basic_package-0.1.0-py3-none-any.whl",
            &fs_err::read(
                context
                    .workspace_root
                    .join("test/links/basic_app-0.1.0-py3-none-any.whl"),
            )?,
            None,
        )
        .await;

    uv_snapshot!(context.filters(), context.sync().arg("--frozen").arg("--index").arg(mirror.index_url()), @"
    exit_code: 1 (failure)
    ----- stderr -----
    error: Failed to download `basic-package==0.1.0`
      cause: Hash mismatch for `basic-package==0.1.0`

             Expected:
               sha256:7b6229db79b5800e4e98a351b5628c1c8a944533a2d428aeeaa7275a30d4ea82

             Computed:
               sha256:c00ab842292ea853ea3918f3fdeac937b2a29ae7572499a8bf096b9a086c6f3e

    hint: `basic-package` (v0.1.0) was included because `project` (v0.1.0) depends on `basic-package`
    ");

    Ok(())
}

/// Match locked files to files on the override index by their normalized filename, since the
/// index may list a file under a non-normalized name (e.g., `SCons-4.8.1-py3-none-any.whl`).
#[tokio::test]
async fn sync_frozen_index_override_normalized_filename() -> Result<()> {
    let context = uv_test::test_context!("3.13");
    let wheel = fs_err::read(
        context
            .workspace_root
            .join("test/links/basic_package-0.1.0-py3-none-any.whl"),
    )?;

    // The lockfile URL must not be used when the file is available on the override index.
    let origin = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&origin)
        .await;
    write_basic_package_lockfile(&context, &origin, "Basic_Package-0.1.0-py3-none-any.whl")?;

    let mirror = PackageServer::new(&"basic-package".parse()?).await;
    mirror
        .serve(
            "Basic_Package-0.1.0-py3-none-any.whl",
            &wheel,
            Some("7b6229db79b5800e4e98a351b5628c1c8a944533a2d428aeeaa7275a30d4ea82"),
        )
        .await;

    uv_snapshot!(context.filters(), context.sync().arg("--frozen").arg("--index").arg(mirror.index_url()), @"
    exit_code: 0 (success)
    ----- stderr -----
    Prepared 1 package in [TIME]
    Installed 1 package in [TIME]
     + basic-package==0.1.0
    ");

    Ok(())
}

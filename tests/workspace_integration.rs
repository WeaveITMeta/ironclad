//! Integration tests for the workspace module.
//!
//! Fully embedded: documents/chunks in Fjall, full-text in tantivy, vectors in
//! embedvec. No database required — each test gets its own temp-dir stores.

use std::sync::Arc;

use ironclaw::workspace::{
    FjallStore, FtsIndex, MockEmbeddings, Repository, SearchConfig, VectorStore, Workspace, paths,
};
use tempfile::TempDir;

/// Build an isolated repository over temp-dir stores. The returned `TempDir`
/// must be kept alive for the duration of the test (it owns the on-disk data).
/// Pass `dim = Some(n)` to enable a vector store of dimension `n`.
async fn make_repo(dim: Option<usize>) -> (Repository, TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let docs = Arc::new(
        FjallStore::open(&dir.path().join("docs").to_string_lossy()).expect("open fjall store"),
    );
    let fts = Arc::new(
        FtsIndex::open(&dir.path().join("fts").to_string_lossy()).expect("open fts index"),
    );
    let vectors = match dim {
        Some(d) => Some(Arc::new(
            VectorStore::open(&dir.path().join("vec").to_string_lossy(), d)
                .await
                .expect("open vector store"),
        )),
        None => None,
    };
    (Repository::new(docs, fts, vectors), dir)
}

#[tokio::test]
async fn test_workspace_write_and_read() {
    let (repo, _tmp) = make_repo(None).await;
    let workspace = Workspace::new("test_write_read", repo);

    let doc = workspace
        .write("README.md", "# Hello World\n\nThis is a test.")
        .await
        .expect("Failed to write");
    assert_eq!(doc.path, "README.md");
    assert!(doc.content.contains("Hello World"));

    let doc2 = workspace.read("README.md").await.expect("Failed to read");
    assert_eq!(doc2.content, "# Hello World\n\nThis is a test.");
}

#[tokio::test]
async fn test_workspace_append() {
    let (repo, _tmp) = make_repo(None).await;
    let workspace = Workspace::new("test_append", repo);

    workspace.write("notes.md", "Line 1").await.expect("write");
    workspace.append("notes.md", "Line 2").await.expect("append");

    let doc = workspace.read("notes.md").await.expect("read");
    assert_eq!(doc.content, "Line 1\nLine 2");
}

#[tokio::test]
async fn test_workspace_nested_paths() {
    let (repo, _tmp) = make_repo(None).await;
    let workspace = Workspace::new("test_nested", repo);

    workspace
        .write("projects/alpha/README.md", "# Alpha")
        .await
        .expect("write alpha");
    workspace
        .write("projects/alpha/notes.md", "Notes here")
        .await
        .expect("write notes");
    workspace
        .write("projects/beta/README.md", "# Beta")
        .await
        .expect("write beta");

    let root = workspace.list("").await.expect("list root");
    assert_eq!(root.len(), 1); // just "projects/"
    assert!(root[0].is_directory);
    assert_eq!(root[0].name(), "projects");

    let projects = workspace.list("projects").await.expect("list projects");
    assert_eq!(projects.len(), 2); // alpha/, beta/

    let alpha = workspace.list("projects/alpha").await.expect("list alpha");
    assert_eq!(alpha.len(), 2); // README.md, notes.md
}

#[tokio::test]
async fn test_workspace_delete() {
    let (repo, _tmp) = make_repo(None).await;
    let workspace = Workspace::new("test_delete", repo);

    workspace.write("temp.md", "temporary").await.expect("write");
    assert!(workspace.exists("temp.md").await.expect("exists"));

    workspace.delete("temp.md").await.expect("delete");
    assert!(!workspace.exists("temp.md").await.expect("exists"));
}

#[tokio::test]
async fn test_workspace_memory_operations() {
    let (repo, _tmp) = make_repo(None).await;
    let workspace = Workspace::new("test_memory_ops", repo);

    workspace
        .append_memory("User prefers dark mode")
        .await
        .expect("append memory");
    workspace
        .append_memory("User's timezone is PST")
        .await
        .expect("append memory");

    let memory = workspace.memory().await.expect("get memory");
    assert!(memory.content.contains("dark mode"));
    assert!(memory.content.contains("PST"));
    assert!(memory.content.contains("\n\n"));
}

#[tokio::test]
async fn test_workspace_daily_log() {
    let (repo, _tmp) = make_repo(None).await;
    let workspace = Workspace::new("test_daily_log", repo);

    workspace
        .append_daily_log("Started working on feature X")
        .await
        .expect("append daily log");

    let log = workspace.today_log().await.expect("today log");
    assert!(log.content.contains("feature X"));
    assert!(log.content.contains("[")); // timestamp prefix
}

#[tokio::test]
async fn test_workspace_fts_search() {
    let (repo, _tmp) = make_repo(None).await;
    let workspace = Workspace::new("test_fts_search", repo);

    workspace
        .write(
            "docs/authentication.md",
            "# Authentication\n\nThe system uses JWT tokens for authentication.",
        )
        .await
        .expect("write");
    workspace
        .write(
            "docs/database.md",
            "# Database\n\nWe use PostgreSQL with pgvector for vector search.",
        )
        .await
        .expect("write");
    workspace
        .write(
            "docs/api.md",
            "# API\n\nThe REST API uses JSON for request and response bodies.",
        )
        .await
        .expect("write");

    let results = workspace
        .search_with_config("JWT authentication", SearchConfig::default().fts_only())
        .await
        .expect("search");
    assert!(!results.is_empty(), "Should find results for JWT");
    assert!(
        results[0].content.contains("JWT"),
        "Top result should contain JWT"
    );

    let results = workspace
        .search_with_config("PostgreSQL database", SearchConfig::default().fts_only())
        .await
        .expect("search");
    assert!(!results.is_empty(), "Should find results for PostgreSQL");
    assert!(
        results[0].content.contains("PostgreSQL"),
        "Top result should contain PostgreSQL"
    );
}

#[tokio::test]
async fn test_workspace_hybrid_search_with_mock_embeddings() {
    // 1536 dims to match the mock embedding provider.
    let (repo, _tmp) = make_repo(Some(1536)).await;
    let embeddings = Arc::new(MockEmbeddings::new(1536));
    let workspace = Workspace::new("test_hybrid_search", repo).with_embeddings(embeddings);

    workspace
        .write("memory.md", "The user prefers dark mode and vim keybindings.")
        .await
        .expect("write");
    workspace
        .write("prefs.md", "Settings: theme=dark, editor=vim, font=monospace")
        .await
        .expect("write");

    let results = workspace
        .search("dark theme preference", 5)
        .await
        .expect("search");
    assert!(!results.is_empty(), "Should find results");
}

#[tokio::test]
async fn test_workspace_list_all() {
    let (repo, _tmp) = make_repo(None).await;
    let workspace = Workspace::new("test_list_all", repo);

    workspace.write("README.md", "root").await.unwrap();
    workspace.write("docs/intro.md", "intro").await.unwrap();
    workspace.write("docs/api/rest.md", "rest").await.unwrap();
    workspace.write("src/main.md", "main").await.unwrap();

    let all = workspace.list_all().await.expect("list_all");
    assert_eq!(all.len(), 4);
    assert!(all.contains(&"README.md".to_string()));
    assert!(all.contains(&"docs/intro.md".to_string()));
    assert!(all.contains(&"docs/api/rest.md".to_string()));
    assert!(all.contains(&"src/main.md".to_string()));
}

#[tokio::test]
async fn test_workspace_system_prompt() {
    let (repo, _tmp) = make_repo(None).await;
    let workspace = Workspace::new("test_system_prompt", repo);

    workspace
        .write(paths::AGENTS, "You are a helpful assistant.")
        .await
        .unwrap();
    workspace
        .write(paths::SOUL, "Be kind and thorough.")
        .await
        .unwrap();
    workspace.write(paths::USER, "Name: Alice").await.unwrap();

    let prompt = workspace.system_prompt().await.expect("system_prompt");
    assert!(prompt.contains("helpful assistant"), "Should include AGENTS.md");
    assert!(prompt.contains("kind and thorough"), "Should include SOUL.md");
    assert!(prompt.contains("Alice"), "Should include USER.md");
}

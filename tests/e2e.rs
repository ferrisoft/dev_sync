//! End-to-end scenarios (design §13.2): the real binary against real git, with bare repositories standing in for
//! GitHub and isolated machines standing in for the laptop and demeter.

#[path = "e2e/support.rs"]
mod support;

use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;

use support::Machine;
use support::Variable;
use support::World;
use support::path_str;


// ============
// === Pair ===
// ============

/// A laptop and demeter sharing a workspace repository, both synced and empty.
struct Pair<'a> {
    laptop: Machine<'a>,
    demeter: Machine<'a>,
}

fn pair(world: &World) -> anyhow::Result<Pair<'_>> {
    let workspace = world.empty_remote("dev2")?;
    let laptop = world.machine("laptop")?;
    laptop.init_workspace(&workspace)?;
    laptop.run(&["push"])?.ok()?;
    let demeter = world.machine("demeter")?;
    demeter.clone_workspace(&workspace)?;
    demeter.run(&["pull"])?.ok()?;
    Ok(Pair { laptop, demeter })
}

/// A pair whose machines both have a clone of `remote`.
struct Sharing<'a> {
    pair: Pair<'a>,
    remote: PathBuf,
}

/// A pair where both machines have a clone of a fresh remote at `path`.
fn pair_sharing<'a>(world: &'a World, path: &str) -> anyhow::Result<Sharing<'a>> {
    let pair = pair(world)?;
    let remote = world.remote("shared")?;
    pair.laptop.clone_into(&remote, path)?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.ok()?;
    anyhow::ensure!(pair.demeter.dev2().join(path).join(".git").is_dir(), "demeter didn't get {path}");
    Ok(Sharing { pair, remote })
}


// ===============
// === Helpers ===
// ===============

fn log_of(machine: &Machine<'_>, repo: &Path) -> anyhow::Result<String> {
    machine.git(repo, &["log", "--format=%s"])
}

fn listing(dir: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in std::fs::read_dir(&next)? {
            let path = entry?.path();
            if path.is_dir() && !path.is_symlink() {
                pending.push(path.clone());
            }
            found.push(path);
        }
    }
    found.sort();
    Ok(found)
}

fn workspace_merging(machine: &Machine<'_>) -> anyhow::Result<bool> {
    Ok(machine.workspace().join(".git").join("MERGE_HEAD").exists())
}

fn names_in(dir: &Path) -> anyhow::Result<Vec<String>> {
    let entries = std::fs::read_dir(dir)?.map(|entry| Ok(entry?.file_name().to_string_lossy().into_owned()));
    let mut names = entries.collect::<anyhow::Result<Vec<_>>>()?;
    names.sort();
    Ok(names)
}


// =================
// === Scenarios ===
// =================

#[test]
fn scenario_01_init_hides_the_workspace_repository_in_the_dev_folder() -> anyhow::Result<()> {
    let world = World::create()?;
    let laptop = world.machine("laptop")?;
    let dev2 = laptop.dev2();
    let remote = world.remote("x")?;
    let existing = laptop.clone_into(&remote, "ferrisoft/x")?;
    let created = laptop.run_in(laptop.dir(), &["init", path_str(&dev2)?], &[])?.ok()?;
    assert!(created.stdout.contains("remote add origin"), "{created:#?}");
    assert_eq!(names_in(&dev2)?, [".dev_sync", "ferrisoft"]);
    assert_eq!(names_in(&laptop.workspace())?, [".git", ".gitattributes", "repos.toml"]);
    assert_eq!(laptop.last_subject()?, "init dev_sync workspace");
    assert_eq!(laptop.git(&laptop.workspace(), &["status", "--porcelain"])?, "");
    let driver = laptop.git(&laptop.workspace(), &["config", "merge.dev-sync.driver"])?;
    assert!(driver.trim().ends_with(" merge-driver %O %A %B %P") && !driver.contains("--root"), "{driver}");
    assert_eq!(laptop.origin(&existing)?, path_str(&remote)?);
    let status = laptop.run(&["status"])?.ok()?;
    assert!(status.stdout.contains("not recorded yet: +ferrisoft/x"), "{status:#?}");
    let again = laptop.run_in(laptop.dir(), &["init", path_str(&dev2)?], &[])?.exits(1)?;
    assert!(again.stderr.contains("already has a .dev_sync folder"), "{again:#?}");
    Ok(())
}

#[test]
fn scenario_02_a_new_clone_reaches_the_other_machine() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let remote = world.remote("x")?;
    pair.laptop.clone_into(&remote, "ferrisoft/x")?;
    pair.laptop.run(&["push"])?.ok()?;
    assert_eq!(pair.laptop.last_subject()?, "laptop: +ferrisoft/x");
    let pulled = pair.demeter.run(&["pull"])?.ok()?;
    assert!(pulled.stdout.contains("✓ cloned ferrisoft/x"), "{pulled:#?}");
    let clone = pair.demeter.dev2().join("ferrisoft").join("x");
    assert_eq!(pair.demeter.origin(&clone)?, path_str(&remote)?);
    Ok(())
}

#[test]
fn scenario_03_a_move_follows_and_keeps_local_commits() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "a")?;
    pair.demeter.commit(&pair.demeter.dev2().join("a"), "local", "work")?;
    std::fs::create_dir_all(pair.laptop.dev2().join("b"))?;
    std::fs::rename(pair.laptop.dev2().join("a"), pair.laptop.dev2().join("b").join("a"))?;
    pair.laptop.run(&["push"])?.ok()?;
    assert_eq!(pair.laptop.last_subject()?, "laptop: a → b/a");
    pair.demeter.run(&["pull"])?.ok()?;
    let moved = pair.demeter.dev2().join("b").join("a");
    assert!(!pair.demeter.dev2().join("a").exists());
    assert!(log_of(&pair.demeter, &moved)?.contains("change local"));
    Ok(())
}

#[test]
fn scenario_04_a_removal_trashes_a_clean_clone() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "a")?;
    std::fs::remove_dir_all(pair.laptop.dev2().join("a"))?;
    pair.laptop.run(&["push"])?.ok()?;
    let pulled = pair.demeter.run(&["pull"])?.ok()?;
    assert!(pulled.stdout.contains("removed a (moved to the Trash)"), "{pulled:#?}");
    assert!(!pair.demeter.dev2().join("a").exists());
    assert!(pair.demeter.trash().join("a").join(".git").is_dir());
    Ok(())
}

#[test]
fn scenario_05_a_removal_with_local_work_is_blocked_until_kept() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "a")?;
    let demeters = pair.demeter.dev2().join("a");
    pair.demeter.commit(&demeters, "local", "work")?;
    std::fs::remove_dir_all(pair.laptop.dev2().join("a"))?;
    pair.laptop.run(&["push"])?.ok()?;
    let pulled = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(pulled.stdout.contains("holds work that exists only here"), "{pulled:#?}");
    assert!(demeters.join(".git").is_dir());
    let status = pair.demeter.run(&["status"])?.exits(2)?;
    assert!(status.stdout.contains("holds work that exists only here"), "{status:#?}");
    pair.demeter.run(&["keep", "a"])?.ok()?;
    assert_eq!(pair.demeter.last_subject()?, "demeter: keep a");
    pair.demeter.run(&["push"])?.ok()?;
    pair.laptop.run(&["pull"])?.ok()?;
    assert!(log_of(&pair.laptop, &pair.laptop.dev2().join("a"))?.contains("change local"));
    Ok(())
}

#[test]
fn scenario_06_a_blocked_removal_goes_ahead_once_the_work_is_pushed() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, remote } = pair_sharing(&world, "a")?;
    let demeters = pair.demeter.dev2().join("a");
    pair.demeter.commit(&demeters, "local", "work")?;
    std::fs::remove_dir_all(pair.laptop.dev2().join("a"))?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.exits(2)?;
    let pushed = pair.demeter.run(&["push"])?.exits(2)?;
    assert!(pushed.stdout.contains("pushed main to origin"), "{pushed:#?}");
    assert!(world.git(&remote, &["log", "--format=%s", "main"])?.contains("change local"));
    pair.demeter.run(&["pull"])?.ok()?;
    assert!(!demeters.exists());
    assert!(pair.demeter.trash().join("a").is_dir());
    Ok(())
}

#[test]
fn scenario_07_a_url_change_reaches_the_other_machine() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, remote } = pair_sharing(&world, "a")?;
    let moved = world.mirror(&remote, "moved")?;
    pair.laptop.git(&pair.laptop.dev2().join("a"), &["remote", "set-url", "origin", path_str(&moved)?])?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.ok()?;
    assert_eq!(pair.demeter.origin(&pair.demeter.dev2().join("a"))?, path_str(&moved)?);
    Ok(())
}

#[test]
fn scenario_08_conflicting_additions_stop_the_pull_until_resolved() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let laptops = world.remote("laptops")?;
    let demeters = world.remote("demeters")?;
    pair.laptop.clone_into(&laptops, "tools")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.clone_into(&demeters, "tools")?;
    let pulled = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(pulled.stdout.contains("`dev_sync pull --continue`"), "{pulled:#?}");
    assert!(pulled.stdout.contains(".dev_sync/repos.toml"), "the resolver must learn where the file is: {pulled:#?}");
    assert!(pulled.stdout.contains("\"tools\": added locally as"), "{pulled:#?}");
    let conflicted = pair.demeter.layout()?;
    assert!(conflicted.contains("<<<<<<< local") && conflicted.contains(">>>>>>> incoming"), "{conflicted}");
    let unresolved = pair.demeter.run(&["pull", "--continue"])?.exits(1)?;
    assert!(unresolved.stderr.contains("conflict marker"), "{unresolved:#?}");
    let keep_incoming = conflicted
        .lines()
        .skip_while(|line| !line.starts_with("<<<<<<<"))
        .skip_while(|line| !line.starts_with("======="))
        .skip(1)
        .take_while(|line| !line.starts_with(">>>>>>>"))
        .collect::<Vec<_>>();
    let before = conflicted.lines().take_while(|line| !line.starts_with("# CONFLICT")).collect::<Vec<_>>();
    let resolved = format!("{}\n{}\n", before.join("\n"), keep_incoming.join("\n"));
    std::fs::write(pair.demeter.workspace().join("repos.toml"), resolved)?;
    std::fs::remove_dir_all(pair.demeter.dev2().join("tools"))?;
    let continued = pair.demeter.run(&["pull", "--continue"])?.ok()?;
    assert!(continued.stdout.contains("✓ cloned tools"), "{continued:#?}");
    assert_eq!(pair.demeter.origin(&pair.demeter.dev2().join("tools"))?, path_str(&laptops)?);
    assert!(!pair.demeter.layout()?.contains("CONFLICT"));
    Ok(())
}

#[test]
fn scenario_08b_a_conflicted_pull_can_be_aborted() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    pair.laptop.clone_into(&world.remote("laptops")?, "tools")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.clone_into(&world.remote("demeters")?, "tools")?;
    pair.demeter.run(&["pull"])?.exits(2)?;
    let head = pair.demeter.head(&pair.demeter.workspace())?;
    pair.demeter.run(&["pull", "--abort"])?.ok()?;
    assert_eq!(pair.demeter.head(&pair.demeter.workspace())?, head);
    assert!(!pair.demeter.layout()?.contains("<<<<<<<"));
    assert_eq!(pair.demeter.git(&pair.demeter.workspace(), &["status", "--porcelain"])?, "");
    Ok(())
}

#[test]
fn scenario_09_concurrent_changes_merge_cleanly() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    pair.laptop.clone_into(&world.remote("x")?, "x")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.clone_into(&world.remote("y")?, "y")?;
    pair.demeter.run(&["pull"])?.ok()?;
    let layout = pair.demeter.layout()?;
    assert!(layout.contains("\"x\" = ") && layout.contains("\"y\" = ") && !layout.contains("<<<<<<<"), "{layout}");
    assert!(pair.demeter.dev2().join("x").join(".git").is_dir());
    Ok(())
}

#[test]
fn scenario_10_content_pull_fast_forwards_and_reports_divergence() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, remote } = pair_sharing(&world, "r")?;
    let laptops = pair.laptop.dev2().join("r");
    let demeters = pair.demeter.dev2().join("r");
    pair.laptop.commit(&laptops, "one", "1")?;
    pair.laptop.git(&laptops, &["push", "--quiet"])?;
    let pulled = pair.demeter.run(&["pull"])?.ok()?;
    assert!(pulled.stdout.contains("r: fast-forwarded main by 1 commit"), "{pulled:#?}");
    assert_eq!(pair.demeter.head(&demeters)?, pair.laptop.head(&laptops)?);
    world.commit_to(&remote, "two", "2")?;
    pair.demeter.commit(&demeters, "mine", "3")?;
    let before = pair.demeter.head(&demeters)?;
    let diverged = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(diverged.stdout.contains("r: main diverged from origin/main (1 ahead, 1 behind)"), "{diverged:#?}");
    assert_eq!(pair.demeter.head(&demeters)?, before);
    pair.demeter.git(&demeters, &["reset", "--quiet", "--hard", "HEAD~1"])?;
    std::fs::write(demeters.join("README"), "dirty")?;
    let dirty = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(dirty.stdout.contains("but has uncommitted changes"), "{dirty:#?}");
    Ok(())
}

#[test]
fn scenario_11_content_push_publishes_only_branches_with_an_upstream() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, remote } = pair_sharing(&world, "r")?;
    let demeters = pair.demeter.dev2().join("r");
    pair.demeter.commit(&demeters, "shared", "1")?;
    pair.demeter.git(&demeters, &["checkout", "--quiet", "-b", "wip"])?;
    pair.demeter.commit(&demeters, "private", "2")?;
    let pushed = pair.demeter.run(&["push"])?.ok()?;
    assert!(pushed.stdout.contains("r: pushed main to origin"), "{pushed:#?}");
    assert!(pushed.stdout.contains("r: wip has no upstream; not pushed"), "{pushed:#?}");
    assert!(world.git(&remote, &["log", "--format=%s", "main"])?.contains("change shared"));
    assert!(world.git(&remote, &["branch", "--list", "wip"])?.is_empty());
    Ok(())
}

#[test]
fn scenario_12_an_unreachable_workspace_remote_changes_nothing() -> anyhow::Result<()> {
    let world = World::create()?;
    let laptop = world.machine("laptop")?;
    let dead = format!("http://127.0.0.1:{}/x.git", support::closed_port()?);
    laptop.run_in(laptop.dir(), &["init", path_str(&laptop.dev2())?], &[])?.ok()?;
    laptop.git(&laptop.workspace(), &["remote", "add", "origin", &dead])?;
    laptop.clone_into(&world.remote("x")?, "x")?;
    let before = listing(&laptop.dev2().join("x"))?;
    let pulled = laptop.run(&["pull"])?.exits(1)?;
    assert!(pulled.stdout.contains("(network, 3 attempts)"), "{pulled:#?}");
    assert_eq!(laptop.last_subject()?, "laptop: +x");
    assert_eq!(listing(&laptop.dev2().join("x"))?, before);
    Ok(())
}

#[test]
fn scenario_13_one_unreachable_repo_does_not_stop_the_others() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let good = world.remote("good")?;
    let later = world.remote("later")?;
    pair.laptop.clone_into(&good, "good")?;
    let broken = pair.laptop.clone_into(&later, "broken")?;
    let missing = world.path().join("remotes").join("missing.git");
    pair.laptop.git(&broken, &["remote", "set-url", "origin", path_str(&missing)?])?;
    pair.laptop.run(&["push"])?.exits(1)?;
    let pulled = pair.demeter.run(&["pull"])?.exits(1)?;
    assert!(pulled.stdout.contains("✓ cloned good"), "{pulled:#?}");
    assert!(pulled.stdout.contains("broken: clone failed (not found)"), "{pulled:#?}");
    pair.laptop.git(&broken, &["remote", "set-url", "origin", path_str(&later)?])?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.ok()?;
    assert_eq!(pair.demeter.origin(&pair.demeter.dev2().join("broken"))?, path_str(&later)?);
    Ok(())
}

#[test]
fn scenario_14_a_stalled_remote_is_stopped() -> anyhow::Result<()> {
    let world = World::create()?;
    let laptop = world.machine("laptop")?;
    let server = support::Server::silent()?;
    laptop.run_in(laptop.dir(), &["init", path_str(&laptop.dev2())?], &[])?.ok()?;
    laptop.git(&laptop.workspace(), &["remote", "add", "origin", &server.url()])?;
    let started = Instant::now();
    let short_limit = Variable { name: "DEV_SYNC_NETWORK_TIMEOUT_SECS", value: OsStr::new("2") };
    let pulled = laptop.run_in(&laptop.dev2(), &["pull"], &[short_limit])?.exits(1)?;
    assert!(started.elapsed() < Duration::from_secs(15), "took {:?}", started.elapsed());
    assert!(pulled.stdout.contains("(stalled)") && pulled.stdout.contains("made no progress for 2 s"), "{pulled:#?}");
    drop(server);
    Ok(())
}

#[test]
fn scenario_15_an_auth_failure_is_not_retried() -> anyhow::Result<()> {
    let world = World::create()?;
    let laptop = world.machine("laptop")?;
    let server = support::Server::unauthorized()?;
    laptop.run_in(laptop.dir(), &["init", path_str(&laptop.dev2())?], &[])?.ok()?;
    laptop.git(&laptop.workspace(), &["remote", "add", "origin", &server.url()])?;
    let pulled = laptop.run(&["pull"])?.exits(1)?;
    assert!(pulled.stdout.contains("(auth)"), "{pulled:#?}");
    assert!(!pulled.stdout.contains("attempts"), "{pulled:#?}");
    drop(server);
    Ok(())
}

#[test]
fn scenario_16_a_local_only_repo_is_reported_and_left_out() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let scratch = pair.laptop.dev2().join("scratch");
    std::fs::create_dir_all(&scratch)?;
    pair.laptop.git(&scratch, &["init", "--quiet"])?;
    let pushed = pair.laptop.run(&["push"])?.ok()?;
    assert!(pushed.stdout.contains("· scratch has no origin remote — it exists only on this machine"), "{pushed:#?}");
    assert!(!pair.laptop.layout()?.contains("scratch"));
    Ok(())
}

#[test]
fn scenario_17_the_merge_driver_merges_files_directly() -> anyhow::Result<()> {
    let world = World::create()?;
    let machine = world.machine("laptop")?;
    let dir = machine.dir();
    let header = "format = 1\n[repos]\n";
    let write = |name: &str, body: &str| std::fs::write(dir.join(name), format!("{header}{body}"));
    write("base", "")?;
    write("local", "\"a\" = { url = \"u1\" }\n")?;
    write("incoming", "\"b\" = { url = \"u2\" }\n")?;
    machine.run_in(dir, &["merge-driver", "base", "local", "incoming", "repos.toml"], &[])?.ok()?;
    let merged = std::fs::read_to_string(dir.join("local"))?;
    assert!(merged.contains("\"a\" = { url = \"u1\" }\n\"b\" = { url = \"u2\" }\n"), "{merged}");
    write("local", "\"t\" = { url = \"u1\" }\n")?;
    write("incoming", "\"t\" = { url = \"u2\" }\n")?;
    let conflicted = machine.run_in(dir, &["merge-driver", "base", "local", "incoming", "repos.toml"], &[])?.exits(1)?;
    assert!(conflicted.stdout.is_empty(), "{conflicted:#?}");
    assert!(std::fs::read_to_string(dir.join("local"))?.contains("<<<<<<< local"));
    std::fs::write(dir.join("base"), "garbage\n")?;
    std::fs::write(dir.join("local"), "garbage\nmine\n")?;
    std::fs::write(dir.join("incoming"), "garbage\n")?;
    machine.run_in(dir, &["merge-driver", "base", "local", "incoming", "repos.toml"], &[])?.ok()?;
    assert_eq!(std::fs::read_to_string(dir.join("local"))?, "garbage\nmine\n");
    std::fs::write(dir.join("incoming"), "theirs\n")?;
    machine.run_in(dir, &["merge-driver", "base", "local", "incoming", "repos.toml"], &[])?.exits(1)?;
    assert!(std::fs::read_to_string(dir.join("local"))?.contains("<<<<<<< local"));
    let broken = machine.run_in(dir, &["merge-driver", "base", "missing", "incoming", "repos.toml"], &[])?;
    assert_eq!(broken.code, 255, "a driver error must not look like a conflict to git: {broken:#?}");
    Ok(())
}

#[test]
fn scenario_18_the_workspace_is_found_from_anywhere_inside_it() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "ferrisoft/app")?;
    let (laptop, dev2) = (&pair.laptop, pair.laptop.dev2());
    laptop.clone_into(&world.remote("tool")?, "ferrisoft/tool")?;
    for cwd in [dev2.join("ferrisoft"), dev2.join("ferrisoft").join("app"), laptop.workspace()] {
        let status = laptop.run_in(&cwd, &["status"], &[])?.ok()?;
        assert!(status.stdout.contains("not recorded yet: +ferrisoft/tool"), "in {}: {status:#?}", cwd.display());
    }
    let outside = laptop.run_in(laptop.dir(), &["status"], &[])?.exits(1)?;
    assert!(outside.stderr.contains("not inside a dev_sync workspace"), "{outside:#?}");
    let pushed = laptop.run_in(laptop.dir(), &["--root", path_str(&dev2)?, "push"], &[])?.ok()?;
    assert!(pushed.stdout.contains("recorded +ferrisoft/tool"), "{pushed:#?}");
    let repository = laptop.run_in(laptop.dir(), &["--root", path_str(&laptop.workspace())?, "status"], &[])?;
    assert!(repository.exits(1)?.stderr.contains("is not a dev_sync workspace"));
    Ok(())
}

#[test]
fn scenario_19_import_records_another_tree_without_touching_it() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let old = pair.laptop.dir().join("old");
    let first = world.remote("first")?;
    let second = world.remote("second")?;
    for (remote, relative) in [(&first, "ferrisoft/w"), (&second, "a")] {
        std::fs::create_dir_all(old.join(relative).parent().ok_or_else(|| anyhow::anyhow!("no parent"))?)?;
        pair.laptop.git(&old, &["clone", "--quiet", path_str(remote)?, relative])?;
    }
    let local = old.join("local");
    std::fs::create_dir_all(&local)?;
    pair.laptop.git(&local, &["init", "--quiet"])?;
    let before = listing(&old)?;
    let imported = pair.laptop.run(&["import", path_str(&old)?])?.ok()?;
    assert!(imported.stdout.contains("local has no origin remote"), "{imported:#?}");
    assert_eq!(pair.laptop.last_subject()?, format!("laptop: import 2 repos from {}", old.display()));
    pair.laptop.run(&["pull"])?.ok()?;
    assert_eq!(pair.laptop.origin(&pair.laptop.dev2().join("ferrisoft").join("w"))?, path_str(&first)?);
    assert_eq!(pair.laptop.origin(&pair.laptop.dev2().join("a"))?, path_str(&second)?);
    assert_eq!(listing(&old)?, before);
    let commits = pair.laptop.commit_count()?;
    for inside in [pair.laptop.dev2().join("ferrisoft"), pair.laptop.dir().to_path_buf()] {
        let refused = pair.laptop.run(&["import", path_str(&inside)?])?.exits(1)?;
        assert!(refused.stderr.contains("the workspace"), "{refused:#?}");
    }
    let missing = pair.laptop.run(&["import", path_str(&old.join("missing"))?])?.exits(1)?;
    assert!(missing.stderr.contains("missing doesn't exist") && !missing.stderr.contains("os error"), "{missing:#?}");
    assert_eq!(pair.laptop.commit_count()?, commits);
    Ok(())
}

#[test]
fn scenario_20_lost_state_is_harmless() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "a")?;
    pair.demeter.clone_into(&world.remote("b")?, "nested/b")?;
    pair.demeter.run(&["push"])?.ok()?;
    let commits = pair.demeter.commit_count()?;
    std::fs::remove_file(pair.demeter.workspace().join(".git").join("dev_sync").join("state.toml"))?;
    pair.demeter.run(&["push"])?.ok()?;
    assert_eq!(pair.demeter.commit_count()?, commits);
    assert!(pair.demeter.dev2().join("a").join(".git").is_dir());
    assert!(pair.demeter.dev2().join("nested").join("b").join(".git").is_dir());
    Ok(())
}

#[test]
fn scenario_21_unusual_paths_survive_everywhere() -> anyhow::Result<()> {
    let world = World::with_unusual_path()?;
    let pair = pair(&world)?;
    let odd = "zażółć gęślą/my \"repo\"";
    let remote = world.remote("odd one")?;
    pair.laptop.clone_into(&remote, odd)?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.ok()?;
    assert_eq!(pair.demeter.origin(&pair.demeter.dev2().join(odd))?, path_str(&remote)?);
    pair.laptop.clone_into(&world.remote("x")?, "x y")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.clone_into(&world.remote("y")?, "y's")?;
    pair.demeter.run(&["pull"])?.ok()?;
    let layout = pair.demeter.layout()?;
    assert!(layout.contains("\"x y\" = ") && layout.contains("\"y's\" = ") && !layout.contains("<<<<<<<"), "{layout}");
    std::fs::rename(pair.laptop.dev2().join(odd), pair.laptop.dev2().join("moved there"))?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.ok()?;
    assert!(pair.demeter.dev2().join("moved there").join(".git").is_dir());
    Ok(())
}

#[test]
fn scenario_21b_workspaces_never_nest() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let (laptop, dev2) = (&pair.laptop, pair.laptop.dev2());
    let team = dev2.join("team");
    let refused = laptop.run_in(laptop.dir(), &["init", path_str(&team)?], &[])?.exits(1)?;
    assert!(refused.stderr.contains("inside the dev_sync workspace"), "{refused:#?}");
    assert!(!team.exists());
    let workspace_remote = world.path().join("remotes").join("dev2.git");
    laptop.git(laptop.dir(), &["clone", "--quiet", path_str(&workspace_remote)?, path_str(&team.join(".dev_sync"))?])?;
    laptop.clone_into(&world.remote("lib")?, "team/lib")?;
    let commits = laptop.commit_count()?;
    let pushed = laptop.run(&["push"])?.exits(1)?;
    assert!(pushed.stderr.contains("holds another dev_sync workspace"), "{pushed:#?}");
    assert_eq!(laptop.commit_count()?, commits);
    laptop.run_in(&team, &["status"], &[])?.ok()?;
    Ok(())
}

#[test]
fn scenario_22_a_repo_mid_merge_is_never_trashed_or_fast_forwarded() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, remote } = pair_sharing(&world, "r")?;
    let demeters = pair.demeter.dev2().join("r");
    pair.demeter.git(&demeters, &["checkout", "--quiet", "-b", "side"])?;
    pair.demeter.commit(&demeters, "README", "side\n")?;
    pair.demeter.git(&demeters, &["checkout", "--quiet", "main"])?;
    pair.demeter.commit(&demeters, "README", "main\n")?;
    let merged = pair.demeter.try_git(&demeters, &["merge", "side"])?;
    anyhow::ensure!(!merged.status.success(), "the merge should conflict");
    world.commit_to(&remote, "upstream", "new")?;
    std::fs::remove_dir_all(pair.laptop.dev2().join("r"))?;
    pair.laptop.run(&["push"])?.ok()?;
    let before = pair.demeter.head(&demeters)?;
    let pulled = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(pulled.stdout.contains("operation in progress"), "{pulled:#?}");
    assert!(demeters.join(".git").join("MERGE_HEAD").is_file());
    assert_eq!(pair.demeter.head(&demeters)?, before);
    Ok(())
}

#[test]
fn scenario_23_interrupted_runs_converge() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, remote } = pair_sharing(&world, "a")?;
    let dev2 = pair.demeter.dev2();
    let unfinished = dev2.join("x").join(".dev_sync-cloning-y-123");
    std::fs::create_dir_all(unfinished.join("objects"))?;
    std::fs::rename(dev2.join("a"), dev2.join(".dev_sync-moving-99-0"))?;
    let stranger = dev2.join(".dev_sync-moving-98-0");
    pair.demeter.git(&dev2, &["clone", "--quiet", path_str(&remote)?, path_str(&stranger)?])?;
    let pulled = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(!unfinished.exists());
    assert!(dev2.join("a").join(".git").is_dir(), "{pulled:#?}");
    assert!(pulled.stdout.contains("put a back after an interrupted move"), "{pulled:#?}");
    assert!(stranger.join(".git").is_dir());
    assert!(pulled.stdout.contains(".dev_sync-moving-98-0"), "{pulled:#?}");
    assert!(pair.demeter.layout()?.contains("\"a\" = "));
    Ok(())
}

#[test]
fn scenario_24_the_lock_keeps_runs_apart() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let lock = pair.demeter.workspace().join(".git").join("dev_sync").join("lock");
    let held = std::fs::File::options().write(true).create(true).truncate(true).open(&lock)?;
    held.try_lock()?;
    std::fs::write(&lock, std::process::id().to_string())?;
    let blocked = pair.demeter.run(&["push"])?.exits(1)?;
    let running = format!("another dev_sync is running (pid {})", std::process::id());
    assert!(blocked.stderr.contains(&running), "{blocked:#?}");
    drop(held);
    let mut child = std::process::Command::new("true").spawn()?;
    let dead = child.id();
    child.wait()?;
    std::fs::write(&lock, dead.to_string())?;
    pair.demeter.run(&["push"])?.ok()?;
    Ok(())
}

#[test]
fn scenario_25_workspace_preconditions_fail_clearly() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let workspace = pair.demeter.workspace();
    pair.demeter.git(&workspace, &["checkout", "--quiet", "--detach"])?;
    let detached = pair.demeter.run(&["push"])?.exits(1)?;
    assert!(detached.stderr.contains("detached"), "{detached:#?}");
    pair.demeter.git(&workspace, &["checkout", "--quiet", "main"])?;
    std::fs::write(workspace.join("repos.toml"), format!("{}# a comment\n", pair.demeter.layout()?))?;
    let edited = pair.demeter.run(&["push"])?.exits(1)?;
    assert!(edited.stderr.contains("repos.toml has uncommitted edits"), "{edited:#?}");
    pair.demeter.git(&workspace, &["checkout", "--", "repos.toml"])?;
    pair.demeter.git(&workspace, &["rm", "--quiet", ".gitattributes"])?;
    pair.demeter.git(&workspace, &["commit", "--quiet", "-m", "drop the merge driver"])?;
    pair.laptop.clone_into(&world.remote("x")?, "x")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.clone_into(&world.remote("y")?, "y")?;
    pair.demeter.run(&["pull"])?.ok()?;
    let merged = pair.demeter.layout()?;
    assert!(merged.contains("\"x\" = ") && merged.contains("\"y\" = "), "{merged}");
    assert!(merged.starts_with("# dev_sync workspace layout"), "not canonical: {merged}");
    pair.laptop.clone_into(&world.remote("laptops")?, "t")?;
    pair.laptop.run(&["pull"])?.ok()?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.clone_into(&world.remote("demeters")?, "t")?;
    pair.demeter.run(&["pull"])?.exits(2)?;
    let conflicted = pair.demeter.layout()?;
    assert!(conflicted.contains("<<<<<<< local"), "{conflicted}");
    pair.demeter.run(&["pull", "--continue"])?.exits(1)?;
    let keep_local = conflicted
        .lines()
        .filter(|line| !["<<<<<<<", ">>>>>>>", "# "].iter().any(|marker| line.starts_with(marker)))
        .take_while(|line| !line.starts_with("======="))
        .map(|line| format!("{line}\n"))
        .collect::<String>();
    std::fs::write(workspace.join("repos.toml"), keep_local)?;
    pair.demeter.run(&["pull", "--continue"])?.ok()?;
    let layout = pair.demeter.layout()?;
    assert!(layout.contains("\"x\" = ") && layout.contains("\"y\" = ") && layout.contains("\"t\" = "), "{layout}");
    Ok(())
}

#[test]
fn scenario_28_a_text_merge_never_commits_an_invalid_layout() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "a-x")?;
    let workspace = pair.demeter.workspace();
    pair.demeter.git(&workspace, &["rm", "--quiet", ".gitattributes"])?;
    pair.demeter.git(&workspace, &["commit", "--quiet", "-m", "drop the merge driver"])?;
    pair.laptop.clone_into(&world.remote("outer")?, "a")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.clone_into(&world.remote("inner")?, "a/b")?;
    let pulled = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(pulled.stdout.contains("conflict:"), "{pulled:#?}");
    assert!(pair.demeter.layout()?.contains("<<<<<<< local"));
    let head = pair.demeter.git(&workspace, &["show", "HEAD:repos.toml"])?;
    assert!(!head.contains("\"a\" = "), "an invalid merge was committed: {head}");
    Ok(())
}

#[test]
fn scenario_29_a_rejected_layout_push_is_recognized_without_git_advice() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    pair.laptop.clone_into(&world.remote("x")?, "x")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.git(&pair.demeter.workspace(), &["config", "advice.pushUpdateRejected", "false"])?;
    pair.demeter.clone_into(&world.remote("y")?, "y")?;
    let pushed = pair.demeter.run(&["push"])?.exits(1)?;
    assert!(pushed.stdout.contains("origin has layout changes you don't have — run `dev_sync pull`"), "{pushed:#?}");
    Ok(())
}

#[test]
fn scenario_30_a_removal_checks_against_the_remote_as_it_is_now() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, remote } = pair_sharing(&world, "r")?;
    let demeters = pair.demeter.dev2().join("r");
    pair.demeter.git(&demeters, &["checkout", "--quiet", "-b", "feat"])?;
    pair.demeter.commit(&demeters, "feature", "work")?;
    pair.demeter.git(&demeters, &["push", "--quiet", "-u", "origin", "feat"])?;
    pair.demeter.git(&demeters, &["checkout", "--quiet", "main"])?;
    world.git(&remote, &["branch", "-D", "feat"])?;
    std::fs::remove_dir_all(pair.laptop.dev2().join("r"))?;
    pair.laptop.run(&["push"])?.ok()?;
    let pulled = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(pulled.stdout.contains("holds work that exists only here"), "{pulled:#?}");
    assert!(demeters.join(".git").is_dir());
    Ok(())
}

#[test]
fn scenario_31_a_url_change_never_repoints_an_unrelated_clone() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, remote } = pair_sharing(&world, "p")?;
    let other = world.remote("unrelated")?;
    std::fs::remove_dir_all(pair.laptop.dev2().join("p"))?;
    pair.laptop.clone_into(&other, "p")?;
    pair.laptop.run(&["push"])?.ok()?;
    let demeters = pair.demeter.dev2().join("p");
    let pulled = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(pulled.stdout.contains("shares no history"), "{pulled:#?}");
    assert_eq!(pair.demeter.origin(&demeters)?, path_str(&remote)?);
    std::fs::create_dir_all(pair.demeter.dev2().join(".old"))?;
    std::fs::rename(&demeters, pair.demeter.dev2().join(".old").join("p"))?;
    pair.demeter.run(&["pull"])?.ok()?;
    assert_eq!(pair.demeter.origin(&demeters)?, path_str(&other)?);
    Ok(())
}

#[test]
fn scenario_32_a_blocked_removal_holds_back_the_moves_into_its_place() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let (one, two, three) = (world.remote("one")?, world.remote("two")?, world.remote("three")?);
    pair.laptop.clone_into(&one, "a")?;
    pair.laptop.clone_into(&two, "b")?;
    pair.laptop.clone_into(&three, "x")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.ok()?;
    let dev2 = pair.demeter.dev2();
    pair.demeter.commit(&dev2.join("x"), "work", "exists only on demeter")?;
    let laptop = pair.laptop.dev2();
    std::fs::remove_dir_all(laptop.join("x"))?;
    std::fs::create_dir(laptop.join("x"))?;
    std::fs::rename(laptop.join("b"), laptop.join("x").join("c"))?;
    std::fs::rename(laptop.join("a"), laptop.join("b"))?;
    pair.laptop.run(&["push"])?.ok()?;
    for _ in 0..2 {
        let pulled = pair.demeter.run(&["pull"])?.exits(2)?;
        let output = &pulled.stdout;
        assert!(output.contains("can't move b → x/c: the repository at x is still there (see above)"), "{pulled:#?}");
        assert!(output.contains("can't move a → b: the repository at b is still there (see above)"), "{pulled:#?}");
        assert_eq!(pair.demeter.origin(&dev2.join("a"))?, path_str(&one)?);
        assert_eq!(pair.demeter.origin(&dev2.join("b"))?, path_str(&two)?);
        let parked = listing(&dev2)?.into_iter().filter(|path| path.to_string_lossy().contains(".dev_sync-moving-"));
        assert_eq!(parked.collect::<Vec<_>>(), Vec::<PathBuf>::new());
    }
    pair.demeter.git(&dev2.join("x"), &["push", "--quiet", "origin", "main"])?;
    pair.demeter.run(&["pull"])?.ok()?;
    assert_eq!(pair.demeter.origin(&dev2.join("b"))?, path_str(&one)?);
    assert_eq!(pair.demeter.origin(&dev2.join("x").join("c"))?, path_str(&two)?);
    assert!(!dev2.join("a").exists());
    Ok(())
}

#[test]
fn scenario_33_a_run_killed_while_committing_the_layout_converges() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let workspace = pair.laptop.workspace();
    let marker = workspace.join(".git").join("dev_sync").join("committing");
    pair.laptop.clone_into(&world.remote("x")?, "x")?;
    let interrupted = format!("# written by a run that was killed\n{}", pair.laptop.layout()?);
    std::fs::write(&marker, &interrupted)?;
    std::fs::write(workspace.join("repos.toml"), &interrupted)?;
    let pushed = pair.laptop.run(&["push"])?.ok()?;
    assert!(pushed.stdout.contains("restored repos.toml"), "{pushed:#?}");
    assert!(pushed.stdout.contains("recorded +x"), "{pushed:#?}");
    assert!(!marker.exists());
    let edited = format!("# edited by hand\n{}", pair.laptop.layout()?);
    std::fs::write(&marker, &interrupted)?;
    std::fs::write(workspace.join("repos.toml"), &edited)?;
    let refused = pair.laptop.run(&["push"])?.exits(1)?;
    assert!(refused.stderr.contains("repos.toml has uncommitted edits"), "{refused:#?}");
    assert_eq!(std::fs::read_to_string(workspace.join("repos.toml"))?, edited);
    Ok(())
}

#[test]
fn scenario_34_one_broken_repository_never_hides_the_rest() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "x")?;
    let dev2 = pair.demeter.dev2();
    pair.demeter.commit(&dev2.join("x"), "work", "exists only here")?;
    std::fs::remove_dir_all(pair.laptop.dev2().join("x"))?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.exits(2)?;
    std::fs::write(dev2.join("x").join(".git").join("index"), "not an index")?;
    let status = pair.demeter.run(&["status"])?.exits(1)?;
    assert!(status.stdout.contains("x was removed from the layout, but checking it failed"), "{status:#?}");
    pair.laptop.clone_into(&world.remote("y")?, "y")?;
    pair.laptop.run(&["push"])?.ok()?;
    let pulled = pair.demeter.run(&["pull"])?;
    assert!(pulled.stdout.contains("✓ cloned y"), "{pulled:#?}");
    assert_ne!(pulled.code, 0, "{pulled:#?}");
    Ok(())
}

#[test]
fn scenario_35_status_during_a_layout_merge_gives_only_advice_that_works() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    pair.laptop.clone_into(&world.remote("first")?, "t")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.clone_into(&world.remote("second")?, "t")?;
    pair.demeter.run(&["pull"])?.exits(2)?;
    let status = pair.demeter.run(&["status"])?.exits(2)?;
    assert!(status.stdout.contains("a layout merge is in progress"), "{status:#?}");
    assert!(!status.stdout.contains("then `dev_sync push`"), "{status:#?}");
    assert!(!status.stdout.contains("uncommitted edits"), "{status:#?}");
    Ok(())
}

#[test]
fn scenario_36_status_never_promises_a_clone_that_pull_can_not_make() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "lib")?;
    let dev2 = pair.demeter.dev2();
    pair.demeter.commit(&dev2.join("lib"), "work", "exists only here")?;
    std::fs::remove_dir_all(pair.laptop.dev2().join("lib"))?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.exits(2)?;
    pair.laptop.clone_into(&world.remote("sub")?, "lib/sub")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.exits(2)?;
    let status = pair.demeter.run(&["status"])?.exits(2)?;
    assert!(!status.stdout.contains("`dev_sync pull` clones it"), "{status:#?}");
    assert!(status.stdout.contains("lib/sub isn't on this machine yet, and pull can't put it there"), "{status:#?}");
    let kept = pair.demeter.run(&["keep", "lib"])?.exits(1)?;
    assert!(kept.stderr.contains("lib/sub") && kept.stderr.contains("push it"), "{kept:#?}");
    assert!(!kept.stderr.contains("RepoPath"), "{kept:#?}");
    Ok(())
}

#[test]
fn scenario_37_quiet_runs_still_say_how_things_are() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "a")?;
    let pulled = pair.demeter.run(&["pull"])?.ok()?;
    assert_eq!(pulled.stdout, "✓ already up to date\n");
    std::fs::write(pair.demeter.dev2().join("a").join(".git").join("config"), "[core\nbroken")?;
    pair.demeter.git(&pair.demeter.workspace(), &["remote", "remove", "origin"])?;
    let status = pair.demeter.run(&["status"])?.exits(1)?;
    assert!(status.stdout.contains("✗ failed to read the origin of"), "{status:#?}");
    assert!(status.stdout.contains("the workspace has no origin remote yet"), "{status:#?}");
    Ok(())
}

#[test]
fn scenario_38_lost_state_never_brings_a_removed_repository_back() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "x")?;
    let dev2 = pair.demeter.dev2();
    pair.demeter.commit(&dev2.join("x"), "work", "exists only here")?;
    std::fs::remove_dir_all(pair.laptop.dev2().join("x"))?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.exits(2)?;
    std::fs::remove_file(pair.demeter.workspace().join(".git").join("dev_sync").join("state.toml"))?;
    let pushed = pair.demeter.run(&["push"])?.exits(2)?;
    assert!(!pushed.stdout.contains("recorded +x"), "{pushed:#?}");
    assert!(pushed.stdout.contains("x was removed from the layout before"), "{pushed:#?}");
    assert!(!pair.demeter.layout()?.contains("\"x\""));
    let status = pair.demeter.run(&["status"])?;
    assert!(status.stdout.contains("x was removed from the layout"), "{status:#?}");
    pair.demeter.run(&["keep", "x"])?.ok()?;
    assert!(pair.demeter.layout()?.contains("\"x\""));
    Ok(())
}

#[test]
fn scenario_39_command_edges() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "p/q")?;
    let dev2 = pair.laptop.dev2();
    let tracked = pair.laptop.git(&pair.laptop.workspace(), &["rev-parse", "--abbrev-ref", "main@{upstream}"])?;
    assert_eq!(tracked.trim(), "origin/main", "the first push sets the upstream");
    pair.laptop.run_in(&dev2.join("p").join("q"), &["status"], &[])?.ok()?;
    pair.laptop.run(&["no-such-command"])?.exits(1)?;
    pair.laptop.run(&["--help"])?.ok()?;
    let version = pair.laptop.run(&["--version"])?.ok()?;
    assert_eq!(version.stdout, format!("dev_sync {}\n", env!("CARGO_PKG_VERSION")));
    let kept = pair.laptop.run(&["keep", "p/q"])?.exits(1)?;
    assert!(kept.stderr.contains("isn't a blocked removal"), "{kept:#?}");
    std::fs::remove_dir_all(dev2.join("p"))?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.run(&["pull"])?.ok()?;
    assert!(!pair.demeter.dev2().join("p").exists(), "the empty parent of a removed repository stays");
    pair.laptop.git(&pair.laptop.workspace(), &["checkout", "--quiet", "--detach"])?;
    let detached = pair.laptop.run(&["status"])?.exits(2)?;
    assert!(detached.stdout.contains("the workspace HEAD is detached"), "{detached:#?}");
    Ok(())
}

#[test]
fn scenario_40_pull_continue_waits_for_every_conflict() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    for (machine, remote, note) in [(&pair.laptop, "first", "laptop\n"), (&pair.demeter, "second", "demeter\n")] {
        let workspace = machine.workspace();
        std::fs::write(workspace.join("notes"), note)?;
        machine.git(&workspace, &["add", "notes"])?;
        machine.git(&workspace, &["commit", "--quiet", "-m", "take notes"])?;
        machine.clone_into(&world.remote(remote)?, "t")?;
    }
    pair.laptop.run(&["push"])?.ok()?;
    let workspace = pair.demeter.workspace();
    let stopped = pair.demeter.run(&["pull"])?.exits(2)?;
    assert!(stopped.stdout.contains("notes has a merge conflict too"), "{stopped:#?}");
    let local = pair.demeter.git(&workspace, &["show", "HEAD:repos.toml"])?;
    std::fs::write(workspace.join("repos.toml"), local)?;
    let waiting = pair.demeter.run(&["pull", "--continue"])?.exits(1)?;
    assert!(waiting.stderr.contains("these files still have conflicts: notes"), "{waiting:#?}");
    pair.demeter.git(&workspace, &["checkout", "--quiet", "--ours", "notes"])?;
    pair.demeter.git(&workspace, &["add", "notes"])?;
    pair.demeter.run(&["pull", "--continue"])?.ok()?;
    assert!(!workspace_merging(&pair.demeter)?);
    Ok(())
}

#[test]
fn scenario_41_a_remote_named_like_an_option_never_reaches_git() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let workspace = pair.laptop.workspace();
    let url = pair.laptop.git(&workspace, &["remote", "get-url", "origin"])?;
    pair.laptop.git(&workspace, &["config", "--", "remote.-x.url", url.trim()])?;
    pair.laptop.git(&workspace, &["config", "--", "remote.-x.fetch", "+refs/heads/*:refs/remotes/-x/*"])?;
    pair.laptop.git(&workspace, &["config", "--", "branch.main.remote", "-x"])?;
    for command in ["push", "pull"] {
        let refused = pair.laptop.run(&[command])?.exits(1)?;
        let output = format!("{}{}", refused.stdout, refused.stderr);
        assert!(output.contains("\"-x\", which git would take for an option"), "{refused:#?}");
    }
    Ok(())
}

#[test]
fn scenario_27_a_closed_stderr_never_crashes_a_verbose_run() -> anyhow::Result<()> {
    let world = World::create()?;
    let Sharing { pair, .. } = pair_sharing(&world, "a")?;
    let mut child = pair
        .demeter
        .command(&pair.demeter.dev2(), &["--verbose", "status"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    drop(child.stderr.take());
    let status = child.wait()?;
    assert_eq!(status.code(), Some(0), "{status:?}");
    Ok(())
}

#[test]
fn scenario_26_a_merge_whose_driver_failed_is_never_completed_without_the_incoming_changes() -> anyhow::Result<()> {
    let world = World::create()?;
    let pair = pair(&world)?;
    let (dev2, workspace) = (pair.demeter.dev2(), pair.demeter.workspace());
    pair.laptop.clone_into(&world.remote("x")?, "x")?;
    pair.laptop.run(&["push"])?.ok()?;
    pair.demeter.clone_into(&world.remote("y")?, "y")?;
    let behind = pair.demeter.run(&["push"])?.exits(1)?;
    assert!(behind.stdout.contains("origin has layout changes you don't have — run `dev_sync pull`"), "{behind:#?}");
    pair.demeter.git(&workspace, &["config", "merge.dev-sync.driver", "false"])?;
    pair.demeter.git(&workspace, &["fetch", "--quiet"])?;
    let merged = pair.demeter.try_git(&workspace, &["merge", "origin/main"])?;
    anyhow::ensure!(!merged.status.success(), "the merge should stop");
    anyhow::ensure!(!pair.demeter.layout()?.contains("\"x\""), "git kept only the local side");
    let refused = pair.demeter.run(&["pull", "--continue"])?.exits(1)?;
    assert!(refused.stderr.contains("-x"), "{refused:#?}");
    assert!(refused.stderr.contains("dev_sync pull --abort"), "{refused:#?}");
    pair.demeter.run(&["pull", "--abort"])?.ok()?;
    pair.demeter.run(&["pull"])?.ok()?;
    let layout = pair.demeter.layout()?;
    assert!(layout.contains("\"x\" = ") && layout.contains("\"y\" = "), "{layout}");
    assert!(dev2.join("x").join(".git").is_dir());
    Ok(())
}

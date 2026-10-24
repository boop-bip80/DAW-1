//! Blocklist and targeted rescans against the fixture, in a process of its own (`harness =
//! false`): the blocklist is process-wide, so the other tests must not see it.

mod common;

use soundcraft_clap_host as host;

fn main() {
    let dir = common::install_fixture();
    let bundle = dir.join("vendor/Fixture.clap");

    // Outside the search path a rescan loads nothing: it never reaches further than a full scan.
    assert!(host::rescan_paths(std::slice::from_ref(&bundle)).is_empty());
    assert!(!host::in_search_path(&bundle));
    host::register_search_dir(dir);
    assert!(host::in_search_path(&bundle));

    // Rescanning one bundle before any scan loads only it, and its plugins can be created at once.
    let found = host::rescan_paths(std::slice::from_ref(&bundle));
    assert_eq!(found.len(), 5);
    let gain = found.iter().find(|d| d.name == "Fixture Gain").expect("fixture gain").id.clone();
    assert!(host::cached().is_none(), "no full scan");
    assert!(host::instantiate(&gain).is_ok());
    assert!(host::cached().is_none(), "creating a rescanned plugin needs no full scan");

    // Relative targets are ignored and leave the known plugins alone.
    assert!(host::rescan_paths(&["Fixture.clap".into(), "vendor/Fixture.clap".into()]).is_empty());
    assert!(host::instantiate(&gain).is_ok());

    // Blocked by name: it can no longer be created or found. (Looking up an id that isn't known
    // runs the full scan, which with SOUNDCRAFT_PLUGIN_PATHS_ONLY=1 covers only the env paths.)
    host::set_blocklist(vec!["fixture".into()]);
    assert!(host::instantiate(&gain).is_err());
    assert!(host::rescan_paths(std::slice::from_ref(&bundle)).is_empty());
    assert!(host::scan().iter().all(|d| d.id != gain));

    // Blocked by folder path, given with a `.` segment.
    host::set_blocklist(vec![dir.join("./vendor")]);
    assert!(host::scan_paths(&[dir.to_path_buf()]).is_empty());

    // Unblocked: back with the next rescan of its folder, in the cached list too.
    host::set_blocklist(Vec::new());
    assert_eq!(host::rescan_paths(&[dir.join("vendor")]).len(), 5);
    assert!(host::cached().is_some_and(|l| l.iter().any(|d| d.id == gain)));
    assert!(host::instantiate(&gain).is_ok());

    // Blocking evicts it from the cached list straight away.
    host::set_blocklist(vec![bundle.clone()]);
    assert!(host::cached().is_some_and(|l| l.iter().all(|d| d.id != gain)));
    assert!(host::instantiate(&gain).is_err());

    // A rebuilt copy elsewhere in the search path, rescanned, wins over the installed bundle.
    host::set_blocklist(Vec::new());
    let dev = dir.join("dev");
    copy_dir(&dir.join("vendor"), &dev);
    host::register_search_dir(&dev);
    let dev_bundle = dev.join("Fixture.clap");
    assert!(host::rescan_paths(std::slice::from_ref(&dev_bundle)).iter().any(|d| d.id == gain && d.path == dev_bundle));
    let path_of = |id: &str| host::cached().and_then(|l| l.into_iter().find(|d| d.id == id)).map(|d| d.path);
    assert_eq!(path_of(&gain), Some(dev_bundle.clone()), "the rescanned copy replaces the installed one");
    assert!(host::instantiate(&gain).is_ok());
    assert_eq!(host::rescan_paths(std::slice::from_ref(&bundle)).len(), 5);
    assert_eq!(path_of(&gain), Some(bundle.clone()), "and rescanning the installed one switches back");

    // A caller's check (the engine passes its read-root check) is asked about every bundle found
    // below a target, not only about the target.
    let asked = std::cell::RefCell::new(Vec::new());
    let none = host::rescan_paths_where(&[dir.to_path_buf()], &|b| {
        asked.borrow_mut().push(b.to_path_buf());
        false
    });
    assert!(none.is_empty());
    assert!(asked.borrow().contains(&bundle) && asked.borrow().contains(&dev_bundle), "{:?}", asked.borrow());
    println!("scan_control: ok");
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

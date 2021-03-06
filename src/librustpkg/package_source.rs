// Copyright 2012-2013 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

extern mod extra;

use target::*;
use package_id::PkgId;
use std::path::Path;
use std::os;
use context::*;
use crate::Crate;
use messages::*;
use source_control::{git_clone, git_clone_general};
use path_util::{find_dir_using_rust_path_hack, default_workspace, make_dir_rwx_recursive};
use util::compile_crate;
use workspace::is_workspace;
use workcache_support;
use extra::workcache;

// An enumeration of the unpacked source of a package workspace.
// This contains a list of files found in the source workspace.
#[deriving(Clone)]
pub struct PkgSrc {
    /// Root of where the package source code lives
    workspace: Path,
    // Directory to start looking in for packages -- normally
    // this is workspace/src/id but it may be just workspace
    start_dir: Path,
    id: PkgId,
    libs: ~[Crate],
    mains: ~[Crate],
    tests: ~[Crate],
    benchs: ~[Crate],
}

impl ToStr for PkgSrc {
    fn to_str(&self) -> ~str {
        fmt!("Package ID %s in start dir %s [workspace = %s]",
             self.id.to_str(),
             self.start_dir.to_str(), self.workspace.to_str())
    }
}
condition! {
    // #6009: should this be pub or not, when #8215 is fixed?
    build_err: (~str) -> ~str;
}

impl PkgSrc {

    pub fn new(workspace: Path, use_rust_path_hack: bool, id: PkgId) -> PkgSrc {
        use conditions::nonexistent_package::cond;

        debug!("Checking package source for package ID %s, \
               workspace = %s", id.to_str(), workspace.to_str());

        let mut to_try = ~[];
        if use_rust_path_hack {
            to_try.push(workspace.clone());
        } else {
            let result = workspace.push("src").push_rel(&id.path.pop()).push(fmt!("%s-%s",
                                                         id.short_name, id.version.to_str()));
            to_try.push(result);
            to_try.push(workspace.push("src").push_rel(&id.path));
        }

        debug!("Checking dirs: %?", to_try.map(|s| s.to_str()).connect(":"));

        let path = to_try.iter().find(|&d| os::path_exists(d));

        let dir: Path = match path {
            Some(d) => (*d).clone(),
            None => {
                // See if any of the prefixes of this package ID form a valid package ID
                // That is, is this a package ID that points into the middle of a workspace?
                for (prefix, suffix) in id.prefixes_iter() {
                    let package_id = PkgId::new(prefix.to_str());
                    let path = workspace.push("src").push_rel(&package_id.path);
                    debug!("in loop: checking if %s is a directory", path.to_str());
                    if os::path_is_dir(&path) {
                        let ps = PkgSrc::new(workspace.clone(),
                                             use_rust_path_hack,
                                             PkgId::new(prefix.to_str()));
                        debug!("pkgsrc: Returning [%s|%s|%s]", workspace.to_str(),
                               ps.start_dir.push_rel(&suffix).to_str(), ps.id.to_str());

                        return PkgSrc {
                            workspace: workspace,
                            start_dir: ps.start_dir.push_rel(&suffix),
                            id: ps.id,
                            libs: ~[],
                            mains: ~[],
                            tests: ~[],
                            benchs: ~[]
                        }

                    };
                }

                // Ok, no prefixes work, so try fetching from git
                let mut ok_d = None;
                for w in to_try.iter() {
                    debug!("Calling fetch_git on %s", w.to_str());
                    let gf = PkgSrc::fetch_git(w, &id);
                    for p in gf.iter() {
                        ok_d = Some(p.clone());
                        break;
                    }
                    if ok_d.is_some() { break; }
                }
                match ok_d {
                    Some(d) => d,
                    None => {
                        if use_rust_path_hack {
                            match find_dir_using_rust_path_hack(&id) {
                                Some(d) => d,
                                None => {
                                    cond.raise((id.clone(),
                                        ~"supplied path for package dir does not \
                                        exist, and couldn't interpret it as a URL fragment"))
                                }
                            }
                        }
                        else {
                            cond.raise((id.clone(),
                                ~"supplied path for package dir does not \
                                exist, and couldn't interpret it as a URL fragment"))
                        }
                    }
                }
            }
        };
        debug!("For package id %s, returning %s", id.to_str(), dir.to_str());

        if !os::path_is_dir(&dir) {
            cond.raise((id.clone(), ~"supplied path for package dir is a \
                                        non-directory"));
        }

        debug!("pkgsrc: Returning {%s|%s|%s}", workspace.to_str(),
               dir.to_str(), id.to_str());

        PkgSrc {
            workspace: workspace,
            start_dir: dir,
            id: id,
            libs: ~[],
            mains: ~[],
            tests: ~[],
            benchs: ~[]
        }
    }

    /// Try interpreting self's package id as a git repository, and try
    /// fetching it and caching it in a local directory. Return the cached directory
    /// if this was successful, None otherwise. Similarly, if the package id
    /// refers to a git repo on the local version, also check it out.
    /// (right now we only support git)
    pub fn fetch_git(local: &Path, pkgid: &PkgId) -> Option<Path> {
        use conditions::failed_to_create_temp_dir::cond;

        // We use a temporary directory because if the git clone fails,
        // it creates the target directory anyway and doesn't delete it

        let scratch_dir = extra::tempfile::mkdtemp(&os::tmpdir(), "rustpkg");
        let clone_target = match scratch_dir {
            Some(d) => d.push("rustpkg_temp"),
            None    => cond.raise(~"Failed to create temporary directory for fetching git sources")
        };

        debug!("Checking whether %s (path = %s) exists locally. Cwd = %s, does it? %?",
               pkgid.to_str(), pkgid.path.to_str(),
               os::getcwd().to_str(),
               os::path_exists(&pkgid.path));

        if os::path_exists(&pkgid.path) {
            debug!("%s exists locally! Cloning it into %s",
                   pkgid.path.to_str(), local.to_str());
            // Ok to use local here; we know it will succeed
            git_clone(&pkgid.path, local, &pkgid.version);
            return Some(local.clone());
        }

        if pkgid.path.components().len() < 2 {
            // If a non-URL, don't bother trying to fetch
            return None;
        }

        let url = fmt!("https://%s", pkgid.path.to_str());
        debug!("Fetching package: git clone %s %s [version=%s]",
                  url, clone_target.to_str(), pkgid.version.to_str());

        if git_clone_general(url, &clone_target, &pkgid.version) {
            // Since the operation succeeded, move clone_target to local.
            // First, create all ancestor directories.
            if make_dir_rwx_recursive(&local.pop())
                && os::rename_file(&clone_target, local) {
                 Some(local.clone())
            }
            else {
                 None
            }
        }
        else {
            None
        }
    }


    // If a file named "pkg.rs" in the start directory exists,
    // return the path for it. Otherwise, None
    pub fn package_script_option(&self) -> Option<Path> {
        let maybe_path = self.start_dir.push("pkg.rs");
        debug!("package_script_option: checking whether %s exists", maybe_path.to_str());
        if os::path_exists(&maybe_path) {
            Some(maybe_path)
        }
        else {
            None
        }
    }

    /// True if the given path's stem is self's pkg ID's stem
    fn stem_matches(&self, p: &Path) -> bool {
        p.filestem().map_default(false, |p| { p == &self.id.short_name.as_slice() })
    }

    fn push_crate(cs: &mut ~[Crate], prefix: uint, p: &Path) {
        assert!(p.components.len() > prefix);
        let mut sub = Path("");
        for c in p.components.slice(prefix, p.components.len()).iter() {
            sub = sub.push(*c);
        }
        debug!("Will compile crate %s", sub.to_str());
        cs.push(Crate::new(&sub));
    }

    /// Infers crates to build. Called only in the case where there
    /// is no custom build logic
    pub fn find_crates(&mut self) {
        use conditions::missing_pkg_files::cond;

        let prefix = self.start_dir.components.len();
        debug!("Matching against %s", self.id.short_name);
        do os::walk_dir(&self.start_dir) |pth| {
            let maybe_known_crate_set = match pth.filename() {
                Some(filename) => match filename {
                    "lib.rs" => Some(&mut self.libs),
                    "main.rs" => Some(&mut self.mains),
                    "test.rs" => Some(&mut self.tests),
                    "bench.rs" => Some(&mut self.benchs),
                    _ => None
                },
                _ => None
            };

            match maybe_known_crate_set {
                Some(crate_set) => PkgSrc::push_crate(crate_set, prefix, pth),
                None => ()
            }
            true
        };

        let crate_sets = [&self.libs, &self.mains, &self.tests, &self.benchs];
        if crate_sets.iter().all(|crate_set| crate_set.is_empty()) {

            note("Couldn't infer any crates to build.\n\
                         Try naming a crate `main.rs`, `lib.rs`, \
                         `test.rs`, or `bench.rs`.");
            cond.raise(self.id.clone());
        }

        debug!("In %s, found %u libs, %u mains, %u tests, %u benchs",
               self.start_dir.to_str(),
               self.libs.len(),
               self.mains.len(),
               self.tests.len(),
               self.benchs.len())
    }

    fn build_crates(&self,
                    ctx: &BuildContext,
                    exec: &mut workcache::Exec,
                    destination_dir: &Path,
                    crates: &[Crate],
                    cfgs: &[~str],
                    what: OutputType) {
        for crate in crates.iter() {
            let path = self.start_dir.push_rel(&crate.file).normalize();
            debug!("build_crates: compiling %s", path.to_str());
            let path_str = path.to_str();
            let cfgs = crate.cfgs + cfgs;

            let result =
                // compile_crate should return the path of the output artifact
                compile_crate(ctx,
                              exec,
                              &self.id,
                              &path,
                              destination_dir,
                              crate.flags,
                              cfgs,
                              false,
                              what).to_str();
            debug!("Result of compiling %s was %s", path_str, result);
        }
    }

    /// Declare all the crate files in the package source as inputs
    pub fn declare_inputs(&self, prep: &mut workcache::Prep) {
        let to_do = ~[self.libs.clone(), self.mains.clone(),
                      self.tests.clone(), self.benchs.clone()];
        for cs in to_do.iter() {
            for c in cs.iter() {
                let path = self.start_dir.push_rel(&c.file).normalize();
                debug!("Declaring input: %s", path.to_str());
                prep.declare_input("file",
                                   path.to_str(),
                                   workcache_support::digest_file_with_date(&path.clone()));
            }
        }
    }

    // It would be better if build returned a Path, but then Path would have to derive
    // Encodable.
    pub fn build(&self,
                 exec: &mut workcache::Exec,
                 build_context: &BuildContext,
                 cfgs: ~[~str]) -> ~str {
        use conditions::not_a_workspace::cond;

        // Determine the destination workspace (which depends on whether
        // we're using the rust_path_hack)
        let destination_workspace = if is_workspace(&self.workspace) {
            debug!("%s is indeed a workspace", self.workspace.to_str());
            self.workspace.clone()
        } else {
            // It would be nice to have only one place in the code that checks
            // for the use_rust_path_hack flag...
            if build_context.context.use_rust_path_hack {
                let rs = default_workspace();
                debug!("Using hack: %s", rs.to_str());
                rs
            } else {
                cond.raise(fmt!("Package root %s is not a workspace; pass in --rust_path_hack \
                                        if you want to treat it as a package source",
                                self.workspace.to_str()))
            }
        };

        let libs = self.libs.clone();
        let mains = self.mains.clone();
        let tests = self.tests.clone();
        let benchs = self.benchs.clone();
        debug!("Building libs in %s, destination = %s",
               destination_workspace.to_str(), destination_workspace.to_str());
        self.build_crates(build_context, exec, &destination_workspace, libs, cfgs, Lib);
        debug!("Building mains");
        self.build_crates(build_context, exec, &destination_workspace, mains, cfgs, Main);
        debug!("Building tests");
        self.build_crates(build_context, exec, &destination_workspace, tests, cfgs, Test);
        debug!("Building benches");
        self.build_crates(build_context, exec, &destination_workspace, benchs, cfgs, Bench);
        destination_workspace.to_str()
    }
}

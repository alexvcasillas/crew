//! Who is on the other end of a connection: a client of yours, or
//! something an agent run started. Requests from inside a run must not be
//! able to steer Crew, so those connections may only hand off and ask.

use std::io;

/// How far up the process tree a connection's peer is followed. Real
/// trees are a handful deep; a longer one is treated as unreadable.
const MAX_DEPTH: usize = 64;

/// What a connection may do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Peer {
    /// The app, the CLI or another client of yours.
    Client,
    /// A process started by an agent run, such as crew-mcp or a command
    /// the agent ran.
    Agent,
}

/// A process's parent and process group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProcInfo {
    pub(crate) ppid: i32,
    pub(crate) pgid: i32,
}

/// Sorts a peer by its process tree. `runs` are the process ids of the
/// live runs, each also its own process group. The peer is an agent if it
/// or any ancestor is a run's process or in a run's group.
///
/// If the tree can't be read (the peer is gone, a lookup fails or the
/// tree is deeper than [`MAX_DEPTH`]), the peer counts as an agent while
/// any run is live, and as a client otherwise: with no run there is no
/// agent to fear.
pub(crate) fn classify(
    peer: Option<i32>,
    runs: &[i32],
    info: impl Fn(i32) -> io::Result<ProcInfo>,
) -> Peer {
    if runs.is_empty() {
        return Peer::Client;
    }
    let Some(mut pid) = peer.filter(|p| *p > 0) else {
        return Peer::Agent;
    };
    for _ in 0..MAX_DEPTH {
        if runs.contains(&pid) {
            return Peer::Agent;
        }
        let Ok(p) = info(pid) else {
            return Peer::Agent;
        };
        if runs.contains(&p.pgid) {
            return Peer::Agent;
        }
        // launchd, or the root of the tree.
        if p.ppid <= 1 || p.ppid == pid {
            return Peer::Client;
        }
        pid = p.ppid;
    }
    Peer::Agent
}

/// The parent and group of a running process.
#[cfg(target_os = "macos")]
pub(crate) fn proc_info(pid: i32) -> io::Result<ProcInfo> {
    // The short form, as the full one is refused for other users'
    // processes, such as the root-owned ones above a terminal.
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdshortinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdshortinfo>() as libc::c_int;
    // SAFETY: the buffer is a `proc_bsdshortinfo` of `size` bytes, which
    // is what PROC_PIDT_SHORTBSDINFO fills.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDT_SHORTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    if got != size {
        let e = io::Error::last_os_error();
        return Err(if got <= 0 && e.raw_os_error().is_some_and(|c| c != 0) {
            e
        } else {
            io::Error::other(format!("no process info for {pid}"))
        });
    }
    // SAFETY: proc_pidinfo filled all `size` bytes.
    let info = unsafe { info.assume_init() };
    Ok(ProcInfo {
        ppid: info.pbsi_ppid as i32,
        pgid: info.pbsi_pgid as i32,
    })
}

/// The parent and group of a running process, from `ps`.
#[cfg(not(target_os = "macos"))]
pub(crate) fn proc_info(pid: i32) -> io::Result<ProcInfo> {
    let out = std::process::Command::new("ps")
        .args(["-o", "ppid=,pgid=", "-p", &pid.to_string()])
        .output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let mut nums = text.split_whitespace().map(str::parse::<i32>);
    match (nums.next(), nums.next()) {
        (Some(Ok(ppid)), Some(Ok(pgid))) => Ok(ProcInfo { ppid, pgid }),
        _ => Err(io::Error::other(format!("no process info for {pid}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn tree(entries: &[(i32, i32, i32)]) -> impl Fn(i32) -> io::Result<ProcInfo> {
        let map: HashMap<i32, ProcInfo> = entries
            .iter()
            .map(|&(pid, ppid, pgid)| (pid, ProcInfo { ppid, pgid }))
            .collect();
        move |pid| {
            map.get(&pid)
                .copied()
                .ok_or_else(|| io::Error::other("gone"))
        }
    }

    #[test]
    fn descendants_and_group_members_of_runs_are_agents() {
        // crewd 100 runs claude 200 (its own group), which runs a shell
        // 300 that runs a script 400. The app is 500.
        let t = tree(&[
            (100, 1, 100),
            (200, 100, 200),
            (300, 200, 200),
            (400, 300, 400),
            (500, 1, 500),
            // Reparented to launchd, still in the run's group.
            (600, 1, 200),
        ]);
        let runs = [200];
        assert_eq!(classify(Some(200), &runs, &t), Peer::Agent);
        assert_eq!(classify(Some(400), &runs, &t), Peer::Agent);
        assert_eq!(classify(Some(600), &runs, &t), Peer::Agent);
        assert_eq!(classify(Some(500), &runs, &t), Peer::Client);
        assert_eq!(classify(Some(100), &runs, &t), Peer::Client);
    }

    #[test]
    fn an_unreadable_tree_is_an_agent_only_while_runs_are_live() {
        let t = tree(&[(300, 999, 300)]);
        // The peer, or an ancestor, can't be read.
        assert_eq!(classify(Some(42), &[200], &t), Peer::Agent);
        assert_eq!(classify(Some(300), &[200], &t), Peer::Agent);
        assert_eq!(classify(None, &[200], &t), Peer::Agent);
        assert_eq!(classify(Some(42), &[], &t), Peer::Client);
        assert_eq!(classify(None, &[], &t), Peer::Client);
        // A loop or a very deep tree is not walked for ever.
        let looped = |pid: i32| -> io::Result<ProcInfo> {
            Ok(ProcInfo {
                ppid: pid + 1,
                pgid: pid,
            })
        };
        assert_eq!(classify(Some(10), &[5], looped), Peer::Agent);
    }

    #[test]
    fn this_process_reads_as_itself() {
        let me = std::process::id() as i32;
        let info = proc_info(me).unwrap();
        assert_eq!(info.ppid, nix::unistd::getppid().as_raw());
        assert_eq!(info.pgid, nix::unistd::getpgrp().as_raw());
        assert!(proc_info(i32::MAX).is_err());
        // Every process above this one reads, up to launchd, so a client
        // started from a terminal is never taken for an agent.
        let mut pid = me;
        for _ in 0..MAX_DEPTH {
            let info = proc_info(pid).unwrap_or_else(|e| panic!("{pid}: {e}"));
            if info.ppid <= 1 {
                break;
            }
            pid = info.ppid;
        }
        assert_eq!(classify(Some(me), &[i32::MAX], proc_info), Peer::Client);
    }
}

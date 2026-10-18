// SPDX-License-Identifier: AGPL-3.0-or-later
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

pub const CPU_STARTUP: &str = "diagnostics.cpu_profile_on_startup";
pub const HEAP_STARTUP: &str = "diagnostics.heap_profile_on_startup";
pub const ARTIFACT_LIMIT: usize = 2 * 1024 * 1024;

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;
// Enable the capability before allocator initialization; stack sampling itself is off.
// No user environment configuration is needed to activate a bounded session later.
#[cfg(all(target_os = "linux", not(target_env = "musl")))]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static mut MALLOC_CONF: *const libc::c_char =
    c"prof:true,prof_active:false,lg_prof_sample:20,narenas:8,thp:never,metadata_thp:disabled,background_thread:true".as_ptr();
// Bound allocator arenas independently of the host CPU count (containers may see many
// more host CPUs than their quota). Purge freed pages even when the server becomes idle.
#[cfg(all(target_os = "linux", target_env = "musl"))]
#[unsafe(export_name = "_rjem_malloc_conf")]
pub static mut MALLOC_CONF: *const libc::c_char =
    c"narenas:8,thp:never,metadata_thp:disabled,background_thread:true".as_ptr();

#[derive(Clone, Default, Serialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct ProfileStatus {
    pub supported: bool,
    pub heap_supported: bool,
    pub running: Option<String>,
    pub ends_at: Option<usize>,
    pub cpu_ready: bool,
    pub heap_ready: bool,
    pub error: Option<String>,
}
#[derive(Clone)]
pub struct Profiles(Arc<Mutex<ProfileStatus>>);
impl Default for Profiles {
    fn default() -> Self {
        Self(Arc::new(Mutex::new(ProfileStatus {
            supported: cfg!(all(target_os = "linux", not(target_env = "musl"))),
            heap_supported: cfg!(all(target_os = "linux", not(target_env = "musl"))),
            ..Default::default()
        })))
    }
}
#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileKind {
    Cpu,
    Heap,
}
impl ProfileKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Heap => "heap",
        }
    }
    pub fn filename(self) -> &'static str {
        match self {
            Self::Cpu => "cpu-profile.svg",
            Self::Heap => "heap-profile.json",
        }
    }
}
pub fn path(db: &Path, kind: ProfileKind) -> PathBuf {
    let mut path = db.as_os_str().to_owned();
    path.push(format!(".{}", kind.filename()));
    path.into()
}
impl Profiles {
    pub fn status(&self) -> ProfileStatus {
        self.0.lock().unwrap().clone()
    }
    pub fn start(&self, db: PathBuf, kind: ProfileKind, seconds: u64) -> anyhow::Result<()> {
        anyhow::ensure!(
            (1..=60).contains(&seconds),
            "capture duration must be 1–60 seconds"
        );
        {
            let mut state = self.0.lock().unwrap();
            anyhow::ensure!(state.supported, "profiling is unavailable on this platform");
            anyhow::ensure!(
                !matches!(kind, ProfileKind::Heap) || state.heap_supported,
                "heap profiling is unavailable in this build; use the Linux Docker build"
            );
            anyhow::ensure!(state.running.is_none(), "a capture is already running");
            state.running = Some(kind.name().into());
            state.ends_at = Some(super::now() as usize + seconds as usize);
            state.error = None;
            match kind {
                ProfileKind::Cpu => state.cpu_ready = false,
                ProfileKind::Heap => state.heap_ready = false,
            }
        }
        let profiles = self.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || capture(&db, kind, seconds)).await;
            let mut state = profiles.0.lock().unwrap();
            state.running = None;
            state.ends_at = None;
            if matches!(result, Ok(Ok(()))) {
                match kind {
                    ProfileKind::Cpu => state.cpu_ready = true,
                    ProfileKind::Heap => state.heap_ready = true,
                }
            } else {
                state.error = Some("Capture failed or exceeded its size limit. Existing playback is unaffected; try a shorter capture.".into());
            }
        });
        Ok(())
    }
}
fn capture(db: &Path, kind: ProfileKind, seconds: u64) -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let bytes = match kind {
            ProfileKind::Cpu => {
                let guard = pprof::ProfilerGuardBuilder::default()
                    .frequency(99)
                    .blocklist(&["libc", "libgcc", "pthread", "vdso"])
                    .build()?;
                std::thread::sleep(Duration::from_secs(seconds));
                let report = guard
                    .report()
                    .frames_post_processor(|frames| {
                        // Thread names can include output IDs; only function stacks are exported.
                        frames.thread_name = "server".into();
                    })
                    .build()?;
                drop(guard);
                let stacks = report.data.into_iter().filter(|(_, count)| *count > 0).map(
                    |(frames, count)| {
                        let names = frames
                            .frames
                            .iter()
                            .rev()
                            .flat_map(|symbols| symbols.iter().rev())
                            .take(64)
                            .map(|symbol| symbol.name())
                            .collect();
                        (names, count as u64)
                    },
                );
                render_flamegraph(stacks)?
            }
            ProfileKind::Heap => capture_heap(db, seconds)?,
        };
        write_artifact(&path(db, kind), &bytes)?;
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (db, kind, seconds);
        anyhow::bail!("profiling unsupported")
    }
}
// A small static flamegraph: widths represent inclusive sample counts, root at bottom.
// Function names are the only profiler metadata serialized; no filenames or thread IDs.
fn render_flamegraph(
    stacks: impl IntoIterator<Item = (Vec<String>, u64)>,
) -> anyhow::Result<Vec<u8>> {
    use std::{collections::BTreeMap, io::Write};
    #[derive(Default)]
    struct Node {
        count: u64,
        children: BTreeMap<String, Node>,
    }
    fn escape(value: &str) -> String {
        value
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    }
    fn draw(
        node: &Node,
        name: &str,
        x: f64,
        width: f64,
        depth: usize,
        height: usize,
        out: &mut LimitedBytes,
    ) -> std::io::Result<()> {
        let y = height - 20 * (depth + 1);
        let title = escape(name);
        let label: String = name.chars().take((width / 7.0).max(0.0) as usize).collect();
        writeln!(
            out,
            "<g><title>{title} ({} samples)</title><rect x=\"{x:.2}\" y=\"{y}\" width=\"{width:.2}\" height=\"19\" fill=\"hsl({},75%,65%)\"/><text x=\"{:.2}\" y=\"{}\" font-size=\"12\">{}</text></g>",
            node.count,
            (depth * 17) % 60,
            x + 2.0,
            y + 14,
            escape(&label)
        )?;
        let mut child_x = x;
        for (name, child) in &node.children {
            let child_width = width * child.count as f64 / node.count.max(1) as f64;
            draw(child, name, child_x, child_width, depth + 1, height, out)?;
            child_x += child_width;
        }
        Ok(())
    }
    let mut root = Node::default();
    let mut nodes = 0;
    let mut max_depth = 0;
    let mut input_bytes = 0usize;
    for (stack_index, (frames, count)) in stacks.into_iter().enumerate() {
        anyhow::ensure!(stack_index < 8192, "profile exceeds stack limit");
        if count == 0 {
            continue;
        }
        root.count += count;
        max_depth = max_depth.max(frames.len().min(64));
        let mut node = &mut root;
        for frame in frames.into_iter().take(64) {
            anyhow::ensure!(frame.len() <= 16384, "profile symbol exceeds size limit");
            input_bytes = input_bytes.saturating_add(frame.len());
            anyhow::ensure!(
                input_bytes <= ARTIFACT_LIMIT,
                "profile input exceeds size limit"
            );
            if !node.children.contains_key(&frame) {
                nodes += 1;
                anyhow::ensure!(nodes <= 8192, "profile exceeds stack limit");
            }
            node = node.children.entry(frame).or_default();
            node.count += count;
        }
    }
    let height = (max_depth + 1) * 20 + 30;
    let mut out = LimitedBytes(Vec::new());
    writeln!(
        out,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1200\" height=\"{height}\" viewBox=\"0 0 1200 {height}\"><rect width=\"100%\" height=\"100%\" fill=\"white\"/><text x=\"4\" y=\"18\" font-size=\"14\">CPU samples — hover for function and count</text>"
    )?;
    draw(&root, "all samples", 0.0, 1200.0, 0, height, &mut out)?;
    out.write_all(b"</svg>")?;
    Ok(out.0)
}

struct LimitedBytes(Vec<u8>);
impl std::io::Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.0.len().saturating_add(bytes.len()) > ARTIFACT_LIMIT {
            return Err(std::io::Error::other("profile exceeds size limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn write_artifact(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    anyhow::ensure!(bytes.len() <= ARTIFACT_LIMIT, "profile exceeds size limit");
    let temp = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut f = options.open(&temp)?;
    f.write_all(bytes)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

#[derive(Serialize)]
struct AllocationStack {
    frames: Vec<String>,
    reported_live_allocations: usize,
    reported_live_bytes: usize,
}
fn parse_heap(text: &str, mut resolve: impl FnMut(usize) -> String) -> Vec<AllocationStack> {
    let mut stacks = Vec::new();
    let mut frames = Vec::new();
    for line in text.lines() {
        if line.starts_with("MAPPED_LIBRARIES:") {
            break;
        }
        if let Some(addresses) = line.strip_prefix("@ ") {
            frames = addresses
                .split_whitespace()
                .take(64)
                .filter_map(|a| usize::from_str_radix(a.trim_start_matches("0x"), 16).ok())
                .map(&mut resolve)
                .collect();
        } else if !frames.is_empty() && line.trim_start().starts_with("t*:") {
            let values: Vec<_> = line
                .trim()
                .strip_prefix("t*:")
                .unwrap_or("")
                .split_whitespace()
                .take(2)
                .filter_map(|v| v.trim_end_matches(':').parse::<usize>().ok())
                .collect();
            if values.len() == 2 {
                stacks.push(AllocationStack {
                    frames: std::mem::take(&mut frames),
                    reported_live_allocations: values[0],
                    reported_live_bytes: values[1],
                });
                if stacks.len() >= 2048 {
                    break;
                }
            }
        }
    }
    stacks
}
#[cfg(target_os = "linux")]
fn capture_heap(db: &Path, seconds: u64) -> anyhow::Result<Vec<u8>> {
    // An RAII guard always disables sampling, including errors and unwinding.
    struct Active;
    impl Drop for Active {
        fn drop(&mut self) {
            unsafe {
                let _ = tikv_jemalloc_ctl::raw::write(b"prof.active\0", false);
            }
        }
    }
    unsafe {
        tikv_jemalloc_ctl::raw::write(b"prof.reset\0", 20usize)?;
        tikv_jemalloc_ctl::raw::write(b"prof.active\0", true)?;
    }
    let _active = Active;
    let before = heap_snapshot(db)?;
    std::thread::sleep(Duration::from_secs(seconds));
    // Stop sampling before symbol resolution and JSON generation, which can be costly.
    unsafe {
        tikv_jemalloc_ctl::raw::write(b"prof.active\0", false)?;
    }
    let after = heap_snapshot(db)?;
    let value = serde_json::json!({"version":env!("CARGO_PKG_VERSION"),"revision":env!("MUSICATA_GIT_SHA"),"duration_seconds":seconds,
        "sampling_interval_bytes":1048576,"scope":"Sampled live allocations originating during this capture; earlier allocations are not attributed. Counts/bytes are sampling estimates, not total RSS. At most 2048 stacks and 64 frames per stack.",
        "stack_limit_reached":before.len() >= 2048 || after.len() >= 2048,
        "frame_limit_reached":before.iter().chain(after.iter()).any(|s| s.frames.len() >= 64),
        "before":before,"after":after});
    let mut writer = LimitedBytes(Vec::new());
    serde_json::to_writer(&mut writer, &value)?;
    Ok(writer.0)
}
#[cfg(target_os = "linux")]
fn heap_snapshot(_db: &Path) -> anyhow::Result<Vec<AllocationStack>> {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};
    // Stream through a pipe: retain at most 2 MiB and drain excess, so even a very
    // allocation-heavy capture cannot leave an unbounded raw profile on disk.
    let mut fds = [0; 2];
    anyhow::ensure!(
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == 0,
        "profile pipe unavailable"
    );
    let mut reader = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let writer = unsafe { std::fs::File::from_raw_fd(fds[1]) };
    let filename = std::ffi::CString::new(format!("/proc/self/fd/{}", writer.as_raw_fd()))?;
    let reading = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut kept = Vec::new();
        let mut buf = [0; 8192];
        loop {
            let n = reader.read(&mut buf)?;
            if n == 0 {
                break;
            }
            let take = n.min(ARTIFACT_LIMIT.saturating_sub(kept.len()));
            kept.extend_from_slice(&buf[..take]);
        }
        Ok(kept)
    });
    let result = unsafe { tikv_jemalloc_ctl::raw::write(b"prof.dump\0", filename.as_ptr()) };
    drop(writer);
    let bytes = reading
        .join()
        .map_err(|_| anyhow::anyhow!("profile reader failed"))??;
    result?;
    anyhow::ensure!(
        bytes.len() < ARTIFACT_LIMIT,
        "heap profile exceeds size limit"
    );
    let text = String::from_utf8(bytes)?;
    let stacks = parse_heap(&text, |address| {
        let mut name = None;
        backtrace::resolve(address as *mut libc::c_void, |symbol| {
            if let Some(n) = symbol.name() {
                name = Some(n.to_string());
            }
        });
        name.unwrap_or_else(|| format!("0x{address:x}"))
    });
    Ok(stacks)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn allocator_policy_bounds_arenas_and_purges_while_idle() {
        let arenas: u32 = unsafe { tikv_jemalloc_ctl::raw::read(b"opt.narenas\0") }.unwrap();
        let purge: bool =
            unsafe { tikv_jemalloc_ctl::raw::read(b"opt.background_thread\0") }.unwrap();
        assert_eq!(arenas, 8);
        assert!(purge);
    }
    #[cfg(target_os = "linux")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    #[ignore = "process-wide sampling must run alone; CI runs this explicitly"]
    async fn actual_profiles_finish_and_resolve_application_stacks() {
        let root =
            std::env::temp_dir().join(format!("musicata-profile-test-{}", super::super::now()));
        std::fs::create_dir_all(&root).unwrap();
        let db = root.join("test.db");
        let profiles = Profiles::default();
        if !profiles.status().supported {
            assert!(profiles.start(db.clone(), ProfileKind::Cpu, 1).is_err());
            assert!(profiles.start(db.clone(), ProfileKind::Heap, 1).is_err());
            assert!(profiles.status().running.is_none());
            std::fs::remove_dir_all(root).unwrap();
            return;
        }
        profiles.start(db.clone(), ProfileKind::Cpu, 2).unwrap();
        assert!(profiles.start(db.clone(), ProfileKind::Heap, 1).is_err());
        let work = std::thread::Builder::new()
            .name("private-output-identity".into())
            .spawn(|| {
                let end = std::time::Instant::now() + Duration::from_secs(3);
                while std::time::Instant::now() < end {
                    cpu_profile_test_workload();
                }
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(15), async {
            while profiles.status().running.is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        work.join().unwrap();
        assert!(profiles.status().cpu_ready, "{:?}", profiles.status().error);
        let svg = std::fs::read_to_string(path(&db, ProfileKind::Cpu)).unwrap();
        assert!(
            svg.contains("cpu_profile_test_workload"),
            "profile did not resolve test function"
        );
        assert!(!svg.contains("private-output"));
        if !profiles.status().heap_supported {
            assert!(profiles.start(db.clone(), ProfileKind::Heap, 1).is_err());
            assert!(profiles.status().running.is_none());
            std::fs::remove_dir_all(root).unwrap();
            return;
        }
        profiles.start(db.clone(), ProfileKind::Heap, 2).unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        let held = heap_profile_test_workload();
        tokio::time::timeout(Duration::from_secs(15), async {
            while profiles.status().running.is_some() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        std::hint::black_box(&held);
        assert!(
            profiles.status().heap_ready,
            "{:?}",
            profiles.status().error
        );
        let heap = std::fs::read_to_string(path(&db, ProfileKind::Heap)).unwrap();
        assert!(
            heap.contains("heap_profile_test_workload"),
            "heap profile did not resolve allocation site: {heap}"
        );
        assert!(!heap.contains("MAPPED_LIBRARIES"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[inline(never)]
    fn cpu_profile_test_workload() {
        let mut value = std::hint::black_box(7u64);
        for i in 0..100000 {
            value = std::hint::black_box(value.wrapping_mul(31).wrapping_add(i));
        }
        std::hint::black_box(value);
    }
    #[inline(never)]
    fn heap_profile_test_workload() -> Vec<Vec<u8>> {
        (0..64)
            .map(|_| std::hint::black_box(vec![42u8; 1024 * 1024]))
            .collect()
    }

    #[test]
    fn heap_export_contains_stack_names_but_no_mapping_paths() {
        let text = "heap_v2/1048576\n@ 0x10 0x20\n  t*: 2: 512 [0: 0]\nMAPPED_LIBRARIES:\n/private/music/secret.flac\n";
        let stacks = parse_heap(text, |address| format!("function_{address}"));
        assert_eq!(stacks.len(), 1);
        assert_eq!(stacks[0].reported_live_bytes, 512);
        let json = serde_json::to_string(&stacks).unwrap();
        assert!(json.contains("function_16"));
        assert!(!json.contains("private"));
    }
    #[test]
    fn flamegraph_merges_stacks_and_escapes_symbols() {
        let svg = render_flamegraph(vec![
            (vec!["root".into(), "a<&>".into()], 3),
            (vec!["root".into(), "b".into()], 1),
        ])
        .unwrap();
        let text = String::from_utf8(svg).unwrap();
        assert!(text.contains("root (4 samples)"));
        assert!(text.contains("a&lt;&amp;&gt; (3 samples)"));
        assert!(text.contains("width=\"900.00\""));
        assert!(!text.contains("a<&>"));
        assert!(render_flamegraph(Vec::new()).is_ok());
        assert!(render_flamegraph(vec![(vec!["x".repeat(ARTIFACT_LIMIT)], 1)]).is_err());
    }
    #[test]
    fn flamegraph_limits_repeated_input_before_rendering() {
        let stacks = (0..3000).map(|_| (vec!["x".repeat(1024)], 1));
        assert!(render_flamegraph(stacks).is_err());
    }
    #[test]
    fn profile_output_is_bounded() {
        use std::io::Write;
        let mut output = LimitedBytes(Vec::new());
        output.write_all(&vec![0; ARTIFACT_LIMIT]).unwrap();
        assert!(output.write_all(b"x").is_err());
    }
    #[tokio::test]
    async fn invalid_capture_duration_does_not_lock_capture() {
        let profiles = Profiles::default();
        assert!(
            profiles
                .start(PathBuf::from("unused"), ProfileKind::Cpu, 61)
                .is_err()
        );
        assert!(profiles.status().running.is_none());
    }
}

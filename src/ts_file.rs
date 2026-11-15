//! Publish a verified TS while preserving payloads and broadcast descriptors.
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    io::{Read, Seek, SeekFrom, Write},
    path::Path,
};
use tokio::io::AsyncReadExt;
pub async fn media(exe: &Path, args: &[&str]) -> Result<(Vec<u8>, bool)> {
    let mut command = tokio::process::Command::new(exe);
    command
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    let mut child = command.spawn()?;
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let a = tokio::spawn(async move {
        let mut b = vec![];
        stdout.take(1048577).read_to_end(&mut b).await?;
        Ok::<_, std::io::Error>(b)
    });
    let b = tokio::spawn(async move {
        let mut b = vec![];
        stderr.take(1048577).read_to_end(&mut b).await?;
        Ok::<_, std::io::Error>(b)
    });
    let result = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let status = child.wait().await?;
        let stdout = a.await??;
        let stderr = b.await??;
        ensure!(
            stdout.len() <= 1048576 && stderr.len() <= 1048576,
            "Verifier output limit exceeded"
        );
        ensure!(status.success(), "Media verification failed");
        Ok::<_, anyhow::Error>((stdout, !stderr.is_empty()))
    })
    .await;
    match result {
        Ok(value) => value,
        Err(_) => {
            let _ = child.kill().await;
            anyhow::bail!("Media verification timed out")
        }
    }
}
fn trim(source: &Path, dest: &Path, pids: &HashSet<u16>) -> Result<usize> {
    let size = std::fs::metadata(source)?.len();
    ensure!(size > 0 && size % 188 == 0, "Unaligned TS");
    let start = size.saturating_sub(8 * 1024 * 1024);
    let start = start - start % 188;
    let mut reader = std::fs::File::open(source)?;
    reader.seek(SeekFrom::Start(start))?;
    let mut tail = vec![];
    reader.read_to_end(&mut tail)?;
    let mut starts = HashMap::new();
    for (i, p) in tail.chunks_exact(188).enumerate() {
        ensure!(p[0] == 0x47 && p[3] & 192 == 0, "Invalid/scrambled TS");
        let pid = ((p[1] & 31) as u16) << 8 | p[2] as u16;
        if pids.contains(&pid) && p[1] & 64 != 0 && p[3] & 16 != 0 {
            let pos = if p[3] & 32 != 0 { 5 + p[4] as usize } else { 4 };
            if p.get(pos..pos + 3) == Some(&[0, 0, 1]) {
                starts.insert(pid, start + i as u64 * 188);
            }
        }
    }
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    reader.seek(SeekFrom::Start(0))?;
    std::io::copy(&mut (&mut reader).take(start), &mut out)?;
    for (i, p) in tail.chunks_exact(188).enumerate() {
        let pid = ((p[1] & 31) as u16) << 8 | p[2] as u16;
        if starts
            .get(&pid)
            .is_none_or(|offset| start + i as u64 * 188 < *offset)
        {
            out.write_all(p)?;
        }
    }
    out.sync_all()?;
    Ok(starts.len())
}
pub async fn finalize(source: &Path, dest: &Path, ffprobe: &Path, ffmpeg: &Path) -> Result<Value> {
    let (raw, _) = media(
        ffprobe,
        &[
            "-v",
            "error",
            "-show_streams",
            "-of",
            "json",
            source
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Non-Unicode media path"))?,
        ],
    )
    .await?;
    let info: Value = serde_json::from_slice(&raw)?;
    let streams = info["streams"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("No streams"))?;
    let video = streams
        .iter()
        .find(|s| s["codec_type"] == "video")
        .ok_or_else(|| anyhow::anyhow!("No video"))?;
    let audio = streams
        .iter()
        .find(|s| s["codec_type"] == "audio")
        .ok_or_else(|| anyhow::anyhow!("No audio"))?;
    let pids = streams
        .iter()
        .filter(|s| s["codec_type"] == "video" || s["codec_type"] == "audio")
        .filter_map(|s| s["id"].as_str())
        .filter_map(|s| u16::from_str_radix(s.trim_start_matches("0x"), 16).ok())
        .collect::<HashSet<_>>();
    let temporary = dest.with_extension("finalizing");
    let src = source.to_owned();
    let tmp = temporary.clone();
    let trimmed = tokio::task::spawn_blocking(move || trim(&src, &tmp, &pids)).await??;
    let (_, warnings) = media(
        ffmpeg,
        &[
            "-nostdin",
            "-v",
            "error",
            "-i",
            temporary.to_str().unwrap(),
            "-t",
            "5",
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-f",
            "null",
            "-",
        ],
    )
    .await?;
    std::fs::hard_link(&temporary, dest)?;
    std::fs::remove_file(&temporary)?;
    std::fs::remove_file(source)?;
    Ok(
        json!({"video":video["codec_name"],"audio":audio["codec_name"],"sample_decode_seconds":5,"sample_decode_warnings":warnings,"trimmed_final_pes_pids":trimmed,"reencoded":false}),
    )
}

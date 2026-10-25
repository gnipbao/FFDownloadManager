//! Suno's normal anonymous playback license and local audio decoding.
//!
//! The official player uses the same-origin `/a` API client, public device and
//! timestamp headers, and an anonymous license supplied by that server. No
//! account cookies or bearer tokens are read, and a permission denial is final.
use aes::{Aes128, Aes192, Aes256};
use aes_gcm::{
    aead::{Aead, KeyInit, Payload},
    Aes256Gcm, Nonce,
};
use anyhow::{anyhow, bail, ensure, Context, Result};
use base64::{engine::general_purpose, Engine};
use ctr::cipher::{KeyIvInit, StreamCipher};
use reqwest::{header, redirect::Policy, Client};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use zeroize::{Zeroize, Zeroizing};

const LICENSE_URL: &str = "https://suno.com/a/api/mango/rights";
const MAX_LICENSE_BYTES: usize = 16 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;

/// Check the source's normal anonymous playback permission for one song.
/// License material is discarded immediately and is never returned to hosts.
pub async fn check_access(content_id: &str) -> Result<()> {
    let id = normalized_id(content_id)?;
    drop(fetch_license(&id).await?);
    Ok(())
}

/// Decode a fully downloaded Suno audio file using a fresh anonymous license.
///
/// The input remains intact for retry. The output must not already exist; a
/// validated, completed temporary file is published without replacing it.
pub async fn decode_file(content_id: &str, input: &Path, output: &Path) -> Result<()> {
    let id = normalized_id(content_id)?;
    let material = fetch_license(&id).await?;
    let input = input.to_path_buf();
    let output = output.to_path_buf();
    tokio::task::spawn_blocking(move || decode_local_file(&material, &input, &output))
        .await
        .context("Suno 音频处理任务未能完成")?
}

#[derive(Deserialize)]
struct WrappedLicense {
    glt: String,
    key: String,
    iv: String,
}

impl Drop for WrappedLicense {
    fn drop(&mut self) {
        self.glt.zeroize();
        self.key.zeroize();
        self.iv.zeroize();
    }
}

struct AudioMaterial {
    key: Zeroizing<Vec<u8>>,
    iv: Zeroizing<Vec<u8>>,
}

async fn fetch_license(content_id: &str) -> Result<AudioMaterial> {
    let mut device = [0_u8; 16];
    getrandom::fill(&mut device).map_err(|_| anyhow!("无法初始化 Suno 临时设备标识"))?;
    device[6] = (device[6] & 0x0f) | 0x40;
    device[8] = (device[8] & 0x3f) | 0x80;
    let hex: String = device.iter().map(|byte| format!("{byte:02x}")).collect();
    let device_id = format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    );
    let timestamp = u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("系统时间无效，无法请求 Suno 播放许可")?
            .as_millis(),
    )
    .context("系统时间超过 Suno 支持范围")?;
    let stamp = serde_json::json!({"timestamp": timestamp}).to_string();
    let browser_token = serde_json::json!({
        "token": general_purpose::STANDARD.encode(stamp.as_bytes())
    })
    .to_string();
    let client = Client::builder()
        .connect_timeout(Duration::from_secs(6))
        .timeout(Duration::from_secs(20))
        .https_only(true)
        .user_agent("FFDownload/0.3.9")
        .redirect(Policy::none())
        .build()
        .map_err(|_| anyhow!("无法初始化 Suno 播放服务连接"))?;
    let mut response = client
        .post(LICENSE_URL)
        .header(header::ORIGIN, "https://suno.com")
        .header(header::REFERER, "https://suno.com/")
        .header(header::ACCEPT, "application/json")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::ACCEPT_ENCODING, "identity")
        .header("Device-Id", device_id)
        .header("Browser-Token", browser_token)
        .body(
            serde_json::json!({
                "content_params": {"content_id": content_id, "content_type": "clip"}
            })
            .to_string(),
        )
        .send()
        .await
        .map_err(|_| anyhow!("无法连接 Suno 公开播放服务，请检查网络后重试"))?;
    match response.status().as_u16() {
        401 | 403 => bail!("Suno 未授予这首歌的匿名播放许可，请在官网确认作品可访问"),
        404 | 410 => bail!("Suno 作品不存在或已删除"),
        429 => bail!("Suno 请求过于频繁，请稍后重试"),
        code => ensure!(
            response.status().is_success(),
            "Suno 播放服务返回 HTTP {code}"
        ),
    }
    ensure!(
        response.content_length().unwrap_or(0) <= MAX_LICENSE_BYTES as u64,
        "Suno 播放许可响应超过读取上限"
    );
    let mut body = Zeroizing::new(Vec::new());
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow!("Suno 播放许可响应读取失败"))?
    {
        ensure!(
            body.len().saturating_add(chunk.len()) <= MAX_LICENSE_BYTES,
            "Suno 播放许可响应超过读取上限"
        );
        body.extend_from_slice(&chunk);
    }
    let wrapped: WrappedLicense =
        serde_json::from_slice(&body).map_err(|_| anyhow!("Suno 播放许可格式无效"))?;
    unwrap_license(content_id, &wrapped)
}

fn normalized_id(raw: &str) -> Result<String> {
    ensure!(
        raw.len() == 36
            && raw.split('-').map(str::len).eq([8, 4, 4, 4, 12])
            && raw
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() || byte == b'-'),
        "Suno 单曲标识无效"
    );
    Ok(raw.to_ascii_lowercase())
}

fn unwrap_license(content_id: &str, wrapped: &WrappedLicense) -> Result<AudioMaterial> {
    ensure!(
        !wrapped.glt.is_empty() && wrapped.glt.len() <= 4096,
        "Suno 匿名播放许可缺少有效会话信息"
    );
    let wrapping_key = Zeroizing::new(<[u8; 32]>::from(Sha256::digest(wrapped.glt.as_bytes())));
    let cipher = Aes256Gcm::new_from_slice(wrapping_key.as_ref())
        .map_err(|_| anyhow!("Suno 播放许可初始化失败"))?;
    let unwrap = |raw: &str| -> Result<Zeroizing<Vec<u8>>> {
        ensure!(raw.len() <= 1024, "Suno 播放许可字段超过读取上限");
        let bytes = Zeroizing::new(
            general_purpose::STANDARD
                .decode(raw)
                .or_else(|_| general_purpose::STANDARD_NO_PAD.decode(raw))
                .map_err(|_| anyhow!("Suno 播放许可编码无效"))?,
        );
        ensure!(bytes.len() >= 28, "Suno 播放许可内容不完整");
        cipher
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad: content_id.as_bytes(),
                },
            )
            .map(Zeroizing::new)
            .map_err(|_| anyhow!("Suno 播放许可校验失败，请重新解析作品"))
    };
    let key = unwrap(&wrapped.key)?;
    let iv = unwrap(&wrapped.iv)?;
    ensure!(
        matches!(key.len(), 16 | 24 | 32) && iv.len() == 16,
        "Suno 播放许可参数无效"
    );
    Ok(AudioMaterial { key, iv })
}

fn decode_local_file(material: &AudioMaterial, input: &Path, output: &Path) -> Result<()> {
    ensure!(!output.exists(), "Suno 音频输出已存在，请使用新的临时文件");
    let mut input = File::open(input).context("无法读取已下载的 Suno 音频")?;
    let parent = output
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut completed = tempfile::Builder::new()
        .prefix(".ffdm-suno-")
        .tempfile_in(parent)
        .context("无法创建 Suno 音频输出临时文件")?;
    decode_stream(material, &mut input, completed.as_file_mut())?;
    completed.flush().context("Suno 音频写入失败")?;
    completed
        .as_file()
        .sync_all()
        .context("Suno 音频保存失败")?;
    completed
        .persist_noclobber(output)
        .map_err(|_| anyhow!("无法完成 Suno 音频保存，目标文件可能已存在"))?;
    Ok(())
}

fn decode_stream<R: Read, W: Write>(
    material: &AudioMaterial,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    match material.key.len() {
        16 => decode_with::<ctr::Ctr128BE<Aes128>, _, _>(material, input, output),
        24 => decode_with::<ctr::Ctr128BE<Aes192>, _, _>(material, input, output),
        32 => decode_with::<ctr::Ctr128BE<Aes256>, _, _>(material, input, output),
        _ => bail!("Suno 音频参数无效"),
    }
}

fn decode_with<C: KeyIvInit + StreamCipher, R: Read, W: Write>(
    material: &AudioMaterial,
    input: &mut R,
    output: &mut W,
) -> Result<()> {
    let mut cipher = C::new_from_slices(&material.key, &material.iv)
        .map_err(|_| anyhow!("Suno 音频处理初始化失败"))?;
    let mut prefix = Zeroizing::new([0_u8; 16]);
    input
        .read_exact(prefix.as_mut())
        .context("Suno 音频文件不完整，已下载内容保留")?;
    cipher
        .try_apply_keystream(prefix.as_mut())
        .map_err(|_| anyhow!("Suno 音频处理超出支持范围"))?;
    let box_size = u32::from_be_bytes(prefix[..4].try_into().unwrap());
    ensure!(
        &prefix[4..8] == b"ftyp"
            && (16..=64 * 1024).contains(&box_size)
            && box_size.is_multiple_of(4)
            && prefix[8..12]
                .iter()
                .all(|byte| byte.is_ascii_graphic() || *byte == b' '),
        "Suno 音频还原后不是有效的 M4A 文件，请重新解析；原下载内容已保留"
    );
    output
        .write_all(prefix.as_ref())
        .context("Suno 音频写入失败")?;
    let mut total = prefix.len() as u64;
    let mut buffer = Zeroizing::new(vec![0_u8; CHUNK_BYTES]);
    loop {
        let size = input.read(&mut buffer).context("读取 Suno 音频文件失败")?;
        if size == 0 {
            break;
        }
        cipher
            .try_apply_keystream(&mut buffer[..size])
            .map_err(|_| anyhow!("Suno 音频处理超出支持范围"))?;
        output
            .write_all(&buffer[..size])
            .context("Suno 音频写入失败")?;
        total += size as u64;
    }
    ensure!(
        total >= u64::from(box_size),
        "Suno 音频文件头不完整，原下载内容已保留"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    const ID: &str = "11111111-2222-4333-8444-555555555555";
    // Independently generated with Node WebCrypto: SHA-256(glt), AES-GCM with
    // AAD=ID and separate 12-byte nonces, then AES-CTR(length=128). The counter
    // begins 0102030405060708fffffffffffffffe to cross the low 64-bit boundary.
    const ENCRYPTED: &str = "SF7+mBITBWiy6dQBPxrJ7I6s7Ji3VDEpwsE6WxLQ2hD3EyyWzAdlmh6GcfJtEPo+OCQkB3aEjOaFbZgKBr7/I0gSQIDVxyRzi1OzWNK5znvRXjsAp6YOzKUDysuvxTPphp6JpRVucq/vV85fY7xrmg4w8P2f3u2DSJdikfqCnc/cyn7SBWHzh3PpZL+Ue3Ae6mQGcjfor53S+sef4DIQtTpZBhFXIZmktxNsPW6KtMwC8ydF1VF7rpRVi+pz98fp/gKVjQkgwc2QyyQUSIWgX1WvfX4z4uH6eOrCDTy+ls3DIFTlCCFOWs27vB9V26FMFpE4yOkp5WNv7Jmvg+eHrGRHKz5tYnEIZC89abpTTOUxazTU84KtNXSxrhFB";

    fn wrapped() -> WrappedLicense {
        WrappedLicense {
            glt: "ffdm-suno-webcrypto-offline-fixture".into(),
            key: "AAECAwQFBgcICQoLdZhlsrX1JiuZHLwaDE4LgcaZEYk0xsC8ji80bcgeH6d+mPGTuVjvwJQHZ0+0pkRa"
                .into(),
            iv: "ICEiIyQlJicoKSorhopqolh5vQJ2gxSS5BF17F+/sU8Q7weA/57FxIDl/2I=".into(),
        }
    }

    fn expected() -> Vec<u8> {
        let mut data = vec![
            0, 0, 0, 28, b'f', b't', b'y', b'p', b'M', b'4', b'A', b' ', 0, 0, 2, 0, b'i', b's',
            b'o', b'm', b'i', b's', b'o', b'2', b'm', b'p', b'4', b'1',
        ];
        data.extend((0..257).map(|i| (i * 17 + 5) as u8));
        data
    }

    struct ShortReads(Cursor<Vec<u8>>);
    impl Read for ShortReads {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let length = buffer.len().min(7);
            self.0.read(&mut buffer[..length])
        }
    }

    #[test]
    fn webcrypto_fixture_matches_wrapping_and_full_width_ctr_across_short_reads() {
        let material = unwrap_license(ID, &wrapped()).unwrap();
        assert_eq!(material.key.as_slice(), &(0..32).collect::<Vec<u8>>());
        assert_eq!(&material.iv[..8], &[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut input = ShortReads(Cursor::new(
            general_purpose::STANDARD.decode(ENCRYPTED).unwrap(),
        ));
        let mut output = Vec::new();
        decode_stream(&material, &mut input, &mut output).unwrap();
        assert_eq!(output, expected());
    }

    #[test]
    fn wrapping_rejects_wrong_song_tampering_and_invalid_identifiers_without_secrets() {
        let data = wrapped();
        let error = unwrap_license("99999999-2222-4333-8444-555555555555", &data)
            .err()
            .unwrap()
            .to_string();
        assert!(!error.contains(&data.glt) && !error.contains(&data.key));
        let mut data = wrapped();
        let mut bytes = general_purpose::STANDARD.decode(&data.iv).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        data.iv = general_purpose::STANDARD.encode(bytes);
        assert!(unwrap_license(ID, &data).is_err());
        for raw in [
            "",
            "../../clip",
            "https://suno.com/song/123",
            "11111111-2222-4333-8444-55555555555g",
        ] {
            assert!(normalized_id(raw).is_err());
        }
    }

    #[test]
    fn file_decode_preserves_input_existing_output_and_cleans_invalid_audio() {
        let directory = tempfile::tempdir().unwrap();
        let input = directory.path().join("encrypted.bin");
        let output = directory.path().join("decoded.m4a");
        let bytes = general_purpose::STANDARD.decode(ENCRYPTED).unwrap();
        std::fs::write(&input, &bytes).unwrap();
        let material = unwrap_license(ID, &wrapped()).unwrap();
        decode_local_file(&material, &input, &output).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), expected());
        assert_eq!(std::fs::read(&input).unwrap(), bytes);
        assert!(decode_local_file(&material, &input, &output).is_err());
        assert_eq!(std::fs::read(&output).unwrap(), expected());
        std::fs::remove_file(&output).unwrap();
        std::fs::write(&input, [0_u8; 64]).unwrap();
        assert!(decode_local_file(&material, &input, &output).is_err());
        assert!(!output.exists());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        std::fs::write(&input, [0_u8; 5]).unwrap();
        assert!(decode_local_file(&material, &input, &output).is_err());
        assert!(!output.exists());
        std::fs::write(&input, &bytes[..20]).unwrap();
        assert!(decode_local_file(&material, &input, &output).is_err());
        assert!(!output.exists());
    }
}

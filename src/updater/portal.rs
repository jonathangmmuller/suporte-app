//! Atualização pelo portal da própria marca (Vancin / TalkInCloud).
//!
//! Substitui por completo o atualizador do RustDesk (api.rustdesk.com e releases do GitHub
//! `rustdesk/rustdesk` nunca são consultados por este app).
//!
//! Fonte: `https://<portal da marca>/downloads/suporte/<pasta da variante>/manifest.json` e
//! `manifest.json.sig`. Marca, portal e variante vêm fixos da compilação:
//!   - marca/portal: `SUPORTE_APP_NAME` (SuporteVancin → portal.vancin.com.br,
//!     SuporteTalkInCloud → portal.talkincloud.com.br);
//!   - variante: `SUPORTE_SOMENTE_ENTRADA=1` → `cliente` (pasta `<marca>/`),
//!     senão `gerencia` (pasta `<marca>-gerencia/`);
//!   - versão do app: `SUPORTE_VERSAO` (derivada da tag do release no build.rs).
//! Sem algum desses valores o atualizador fica desligado (falha fechada).
//!
//! Regras de aceitação (todas obrigatórias):
//!   1. assinatura Ed25519 válida de `CONTEXTO || manifest.json` com uma chave embutida;
//!   2. `schema == 1`; `brand`, `variant` e `host` presentes e iguais aos da compilação;
//!   3. `app_version` estritamente maior que a do app (sem downgrade, sem reinstalar a mesma);
//!   4. arquivo do sistema/arquitetura com nome simples, sha256 hex de 64 e tamanho plausível;
//!   5. a URL de download é montada aqui (mesmo host e pasta), nunca lida do manifesto;
//!   6. só HTTPS, sem seguir redirecionamento; o sha256/tamanho do instalador são conferidos
//!      contra o manifesto assinado antes de qualquer execução.

use hbb_common::{
    anyhow::{anyhow, Context},
    bail,
    base64::{engine::general_purpose::STANDARD, Engine as _},
    log,
    sodiumoxide::crypto::sign::{verify_detached, PublicKey, Signature},
    ResultType,
};
use serde_derive::Deserialize;
use std::{
    io::Read,
    path::{Path, PathBuf},
    time::Duration,
};

/// Prefixo assinado junto com o manifesto (igual ao de `assinar-manifesto-suporte.sh`).
pub(crate) const SIGNATURE_CONTEXT: &[u8] = b"suporte-app manifest v1\n";

/// Chaves públicas aceitas (Ed25519, 32 bytes). A privada fica só no Chaves do macOS
/// (item `vancin-suporte-atualizacao-ed25519`). Para trocar de chave: publicar um app com as
/// duas chaves, esperar todos atualizarem, então assinar com a nova e retirar a antiga.
const TRUSTED_KEYS: &[[u8; 32]] = &[
    // d8d3AaGo1jerzp5PkebQNrlFsUgz4zNJZ0h/g6G/wPU= (2026-11-27)
    [
        119, 199, 119, 1, 161, 168, 214, 55, 171, 206, 158, 79, 145, 230, 208, 54, 185, 69, 177,
        72, 51, 227, 51, 73, 103, 72, 127, 131, 161, 191, 192, 245,
    ],
];

const MANIFEST_SCHEMA: u32 = 1;
const MANIFEST_MAX_BYTES: u64 = 64 * 1024;
const SIGNATURE_MAX_BYTES: u64 = 1024;
const ARTIFACT_MAX_BYTES: u64 = 512 * 1024 * 1024;
const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Intervalo base entre consultas e variação aleatória (± JITTER) para espalhar os clientes.
const CHECK_INTERVAL: Duration = Duration::from_secs(4 * 60 * 60);
const CHECK_JITTER: Duration = Duration::from_secs(60 * 60);
/// Primeira consulta: 30 s após abrir + até 3 min aleatórios.
const INITIAL_DELAY: Duration = Duration::from_secs(30);
const INITIAL_JITTER: Duration = Duration::from_secs(3 * 60);
/// Nova tentativa depois de falha ou de sessão ativa: 30 min + até 10 min.
const RETRY_DELAY: Duration = Duration::from_secs(30 * 60);
const RETRY_JITTER: Duration = Duration::from_secs(10 * 60);
/// Não reinstalar a mesma versão mais de uma vez nesse período (evita laço se a instalação
/// falhar ou se o build publicado tiver versão interna errada).
#[cfg_attr(not(windows), allow(dead_code))]
const SAME_VERSION_BACKOFF_SECS: u64 = 24 * 60 * 60;
#[cfg_attr(not(windows), allow(dead_code))]
const ATTEMPT_MARKER: &str = "atualizacao-portal-tentativa";

/// Identidade fixa da compilação.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BuildIdentity {
    pub brand: &'static str,
    pub host: &'static str,
    pub variant: &'static str,
    pub version: (u32, u32, u32),
}

impl BuildIdentity {
    pub fn folder(&self) -> String {
        if self.variant == "gerencia" {
            format!("{}-gerencia", self.brand)
        } else {
            self.brand.to_owned()
        }
    }

    pub fn base_url(&self) -> String {
        format!("https://{}/downloads/suporte/{}/", self.host, self.folder())
    }
}

pub(crate) fn brand_from_app_name(app_name: &str) -> Option<(&'static str, &'static str)> {
    match app_name {
        "SuporteVancin" => Some(("vancin", "portal.vancin.com.br")),
        "SuporteTalkInCloud" => Some(("talkincloud", "portal.talkincloud.com.br")),
        _ => None,
    }
}

pub(crate) fn parse_version(v: &str) -> Option<(u32, u32, u32)> {
    let mut it = v.split('.');
    let mut part = || -> Option<u32> {
        let s = it.next()?;
        if s.is_empty() || s.len() > 6 || !s.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        s.parse().ok()
    };
    let r = (part()?, part()?, part()?);
    if it.next().is_some() {
        return None;
    }
    Some(r)
}

pub(crate) fn version_string(v: (u32, u32, u32)) -> String {
    format!("{}.{}.{}", v.0, v.1, v.2)
}

/// Identidade desta compilação, ou `None` (atualizador desligado).
pub(crate) fn build_identity() -> Option<BuildIdentity> {
    let (brand, host) = brand_from_app_name(option_env!("SUPORTE_APP_NAME")?)?;
    let version = parse_version(option_env!("SUPORTE_VERSAO")?)?;
    let variant = if option_env!("SUPORTE_SOMENTE_ENTRADA") == Some("1") {
        "cliente"
    } else {
        "gerencia"
    };
    Some(BuildIdentity {
        brand,
        host,
        variant,
        version,
    })
}

/// Cabeçalho do relatório `--suporte-build-info` (lido pelo CI, pelo publicador e pela troca
/// do Windows para conferir que o binário é da marca/variante/versão esperadas).
pub(crate) const BUILD_INFO_HEADER: &str = "suporte-build-info v1";

/// Relatório da identidade compilada. `incoming_only` vem do `hbb_common` (o que de fato
/// decide se a build só recebe acesso); tem de bater com a variante.
pub(crate) fn build_info_text(id: Option<&BuildIdentity>, incoming_only: bool) -> ResultType<String> {
    let id = id.ok_or_else(|| anyhow!("build sem identidade (SUPORTE_APP_NAME/SUPORTE_VERSAO)"))?;
    if (id.variant == "cliente") != incoming_only {
        bail!(
            "variante {} incoerente com incoming_only={}",
            id.variant,
            incoming_only
        );
    }
    Ok(format!(
        "{BUILD_INFO_HEADER}\nbrand={}\nvariant={}\nincoming_only={}\nversion={}\n",
        id.brand,
        id.variant,
        if incoming_only { 1 } else { 0 },
        version_string(id.version)
    ))
}

/// `--suporte-build-info <arquivo>`: grava o relatório desta build.
pub(crate) fn write_build_info(path: &str) -> ResultType<()> {
    let t = build_info_text(
        build_identity().as_ref(),
        hbb_common::config::is_incoming_only(),
    )?;
    std::fs::write(path, t)?;
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BuildInfo {
    pub brand: String,
    pub variant: String,
    pub incoming_only: bool,
    pub version: (u32, u32, u32),
}

impl BuildInfo {
    /// `marca:variante:versão`, formato do `--suporte-esperado`.
    pub fn esperado(&self) -> String {
        format!("{}:{}:{}", self.brand, self.variant, version_string(self.version))
    }
}

pub(crate) fn parse_build_info(t: &str) -> ResultType<BuildInfo> {
    let mut linhas = t.lines();
    if linhas.next().map(str::trim) != Some(BUILD_INFO_HEADER) {
        bail!("relatório de build sem cabeçalho");
    }
    let mut campos = std::collections::BTreeMap::new();
    for l in linhas.map(str::trim).filter(|l| !l.is_empty()) {
        let (k, v) = l.split_once('=').ok_or_else(|| anyhow!("linha inválida: {l}"))?;
        if campos.insert(k, v).is_some() {
            bail!("campo repetido: {k}");
        }
    }
    let campo = |k: &str| campos.get(k).copied().ok_or_else(|| anyhow!("sem {k}"));
    let brand = campo("brand")?;
    if !matches!(brand, "vancin" | "talkincloud") {
        bail!("marca desconhecida: {brand}");
    }
    let variant = campo("variant")?;
    let incoming_only = match campo("incoming_only")? {
        "1" => true,
        "0" => false,
        v => bail!("incoming_only inválido: {v}"),
    };
    match (variant, incoming_only) {
        ("cliente", true) | ("gerencia", false) => {}
        _ => bail!("variante {variant} incoerente com incoming_only={incoming_only}"),
    }
    let v = campo("version")?;
    let version = parse_version(v).ok_or_else(|| anyhow!("versão inválida: {v}"))?;
    Ok(BuildInfo {
        brand: brand.to_owned(),
        variant: variant.to_owned(),
        incoming_only,
        version,
    })
}

/// O que o app instalado espera do instalador que vai rodar: mesma marca e variante, versão
/// do manifesto assinado.
pub(crate) fn esperado_para(id: &BuildIdentity, app_version: &str) -> String {
    format!("{}:{}:{}", id.brand, id.variant, app_version)
}

/// Recusa um binário cujo relatório não é o esperado (`marca:variante:versão`).
pub(crate) fn conferir_esperado(info: &BuildInfo, esperado: &str) -> ResultType<()> {
    let e = esperado.trim();
    if e.is_empty() || info.esperado() != e {
        bail!(
            "instalador de outra build: é {}, esperado {}",
            info.esperado(),
            e
        );
    }
    Ok(())
}

/// A própria build confere o `--suporte-esperado` recebido (no `--update`, antes de tocar
/// em qualquer coisa).
pub(crate) fn conferir_esta_build(esperado: &str) -> ResultType<()> {
    let t = build_info_text(
        build_identity().as_ref(),
        hbb_common::config::is_incoming_only(),
    )?;
    conferir_esperado(&parse_build_info(&t)?, esperado)
}

/// (os, arch) como aparecem no manifesto, para este binário.
pub(crate) fn current_target() -> Option<(&'static str, &'static str)> {
    target_for(std::env::consts::OS, std::env::consts::ARCH)
}

pub(crate) fn target_for(os: &str, arch: &str) -> Option<(&'static str, &'static str)> {
    match (os, arch) {
        ("windows", "x86_64") => Some(("windows", "x64")),
        ("windows", "aarch64") => Some(("windows", "arm64")),
        ("macos", "aarch64") => Some(("macos", "arm64")),
        ("linux", "x86_64") => Some(("linux", "x64")),
        _ => None,
    }
}

#[derive(Deserialize)]
struct Manifest {
    schema: Option<u32>,
    brand: String,
    variant: Option<String>,
    /// Versão interna do RustDesk (CFBundleShortVersionString no macOS), ex.: 1.5.1.
    version: Option<String>,
    /// Versão do app de suporte, ex.: 1.0.6 (vem da tag do release).
    app_version: Option<String>,
    host: Option<String>,
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
struct ManifestFile {
    os: String,
    arch: String,
    file: String,
    size: u64,
    sha256: String,
}

/// Instalador escolhido num manifesto já verificado.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PortalArtifact {
    pub app_version: String,
    pub bundle_version: String,
    pub file_name: String,
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Manifesto válido, mas sem versão mais nova.
    UpToDate,
    Update(PortalArtifact),
}

fn decode_signature(sig: &[u8]) -> ResultType<Signature> {
    let text = std::str::from_utf8(sig).map_err(|_| anyhow!("assinatura não é texto"))?;
    let raw = STANDARD
        .decode(text.trim())
        .map_err(|_| anyhow!("assinatura não é base64"))?;
    if raw.len() != 64 {
        bail!("assinatura com tamanho inválido ({} bytes)", raw.len());
    }
    let mut b = [0u8; 64];
    b.copy_from_slice(&raw);
    Signature::from_bytes(&b).map_err(|_| anyhow!("assinatura inválida"))
}

fn signature_ok(manifest: &[u8], sig: &Signature, keys: &[[u8; 32]]) -> bool {
    let mut msg = Vec::with_capacity(SIGNATURE_CONTEXT.len() + manifest.len());
    msg.extend_from_slice(SIGNATURE_CONTEXT);
    msg.extend_from_slice(manifest);
    keys.iter()
        .any(|k| verify_detached(sig, &msg, &PublicKey(*k)))
}

fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Verificação completa (sem rede). `keys` é parâmetro para os testes.
pub(crate) fn verify_manifest_with_keys(
    manifest_bytes: &[u8],
    signature_bytes: &[u8],
    id: &BuildIdentity,
    target: (&str, &str),
    keys: &[[u8; 32]],
) -> ResultType<Verdict> {
    if signature_bytes.is_empty() {
        bail!("manifesto sem assinatura");
    }
    let sig = decode_signature(signature_bytes)?;
    if !signature_ok(manifest_bytes, &sig, keys) {
        bail!("assinatura do manifesto não confere");
    }
    let m: Manifest = serde_json::from_slice(manifest_bytes).context("manifesto não é JSON")?;
    if m.schema != Some(MANIFEST_SCHEMA) {
        bail!("schema do manifesto não suportado: {:?}", m.schema);
    }
    if m.brand != id.brand {
        bail!("manifesto de outra marca: {}", m.brand);
    }
    if m.variant.as_deref() != Some(id.variant) {
        bail!("manifesto de outra variante: {:?}", m.variant);
    }
    match m.host.as_deref() {
        None => bail!("manifesto sem host"),
        Some(h) if h != id.host => bail!("manifesto de outro host: {}", h),
        Some(_) => {}
    }
    let app_version = m
        .app_version
        .as_deref()
        .ok_or_else(|| anyhow!("manifesto sem app_version"))?;
    let new_version =
        parse_version(app_version).ok_or_else(|| anyhow!("app_version inválida: {app_version}"))?;
    if new_version <= id.version {
        return Ok(Verdict::UpToDate);
    }
    let (os, arch) = target;
    let f = m
        .files
        .iter()
        .find(|f| f.os == os && f.arch == arch)
        .ok_or_else(|| anyhow!("manifesto sem instalador para {os}/{arch}"))?;
    if !is_plain_file_name(&f.file) {
        bail!("nome de arquivo inválido no manifesto: {}", f.file);
    }
    if !is_sha256_hex(&f.sha256) {
        bail!("sha256 inválido no manifesto");
    }
    if f.size == 0 || f.size > ARTIFACT_MAX_BYTES {
        bail!("tamanho inválido no manifesto: {}", f.size);
    }
    let url = format!("{}{}", id.base_url(), f.file);
    check_same_origin(&url, id)?;
    Ok(Verdict::Update(PortalArtifact {
        app_version: version_string(new_version),
        bundle_version: m.version.clone().unwrap_or_default(),
        file_name: f.file.clone(),
        url,
        size: f.size,
        sha256: f.sha256.clone(),
    }))
}

pub(crate) fn verify_manifest(
    manifest_bytes: &[u8],
    signature_bytes: &[u8],
    id: &BuildIdentity,
    target: (&str, &str),
) -> ResultType<Verdict> {
    verify_manifest_with_keys(manifest_bytes, signature_bytes, id, target, TRUSTED_KEYS)
}

/// A URL precisa ser exatamente `https://<host da marca>/downloads/suporte/<pasta>/<arquivo>`.
pub(crate) fn check_same_origin(url: &str, id: &BuildIdentity) -> ResultType<()> {
    let u = url::Url::parse(url).context("URL inválida")?;
    let expected_prefix = id.base_url();
    if u.scheme() != "https"
        || u.host_str() != Some(id.host)
        || u.port().is_some()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
        || !url.starts_with(&expected_prefix)
        || u.as_str() != url
    {
        bail!("URL fora do portal da marca: {url}");
    }
    let rest = &url[expected_prefix.len()..];
    if !is_plain_file_name(rest) {
        bail!("URL fora da pasta da variante: {url}");
    }
    Ok(())
}

// ---------------------------------------------------------------- rede

/// Cliente HTTPS sem redirecionamento e sem aceitar certificado inválido.
fn http_client(native_tls: bool, timeout: Duration) -> ResultType<reqwest::blocking::Client> {
    let ua = format!(
        "{}/{} (atualizacao)",
        option_env!("SUPORTE_APP_NAME").unwrap_or("suporte"),
        option_env!("SUPORTE_VERSAO").unwrap_or("0")
    );
    let b = reqwest::blocking::Client::builder()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(15))
        .timeout(timeout)
        .user_agent(ua);
    let b = if native_tls {
        b.use_native_tls()
    } else {
        b.use_rustls_tls()
    };
    Ok(b.build()?)
}

fn read_limited<R: Read>(r: R, limit: u64, what: &str) -> ResultType<Vec<u8>> {
    let mut out = Vec::new();
    r.take(limit + 1).read_to_end(&mut out)?;
    if out.len() as u64 > limit {
        bail!("{what} maior que {limit} bytes");
    }
    Ok(out)
}

fn get_small(
    client: &reqwest::blocking::Client,
    url: &str,
    limit: u64,
    what: &str,
) -> ResultType<Vec<u8>> {
    let resp = client.get(url).send()?;
    let status = resp.status();
    if status.is_redirection() {
        bail!("{what}: redirecionamento recusado ({status})");
    }
    if status != reqwest::StatusCode::OK {
        bail!("{what}: HTTP {status}");
    }
    read_limited(resp, limit, what)
}

/// Baixa e verifica o manifesto. Devolve também se o TLS nativo foi necessário.
fn fetch_verdict(id: &BuildIdentity) -> ResultType<(Verdict, bool)> {
    let target = current_target().ok_or_else(|| anyhow!("sistema/arquitetura sem atualização"))?;
    let base = id.base_url();
    let manifest_url = format!("{base}manifest.json");
    let sig_url = format!("{base}manifest.json.sig");
    let mut last_err = None;
    for native in [false, true] {
        let client = http_client(native, HTTP_TIMEOUT)?;
        match get_small(&client, &manifest_url, MANIFEST_MAX_BYTES, "manifest.json") {
            Ok(manifest) => {
                let sig = get_small(&client, &sig_url, SIGNATURE_MAX_BYTES, "manifest.json.sig")?;
                return Ok((verify_manifest(&manifest, &sig, id, target)?, native));
            }
            Err(e) => {
                // Só tenta o TLS nativo quando a falha foi de conexão/TLS.
                let is_request = e
                    .downcast_ref::<reqwest::Error>()
                    .map_or(false, |e| e.is_connect() || e.is_request());
                if !is_request {
                    return Err(e);
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("falha ao consultar o portal")))
}

/// Consulta o portal. `Ok(None)` = já está na versão mais nova (ou atualizador desligado).
pub(crate) fn check() -> ResultType<Option<(PortalArtifact, bool)>> {
    let Some(id) = build_identity() else {
        log::info!(
            "[portal-update] atualizador desligado (marca/versão não definidas na compilação)"
        );
        return Ok(None);
    };
    match fetch_verdict(&id)? {
        (Verdict::UpToDate, _) => {
            log::debug!(
                "[portal-update] versão {} já é a mais nova",
                version_string(id.version)
            );
            Ok(None)
        }
        (Verdict::Update(a), native) => {
            log::info!(
                "[portal-update] nova versão {} (atual {})",
                a.app_version,
                version_string(id.version)
            );
            Ok(Some((a, native)))
        }
    }
}

/// Baixa o instalador para `file_path` conferindo tamanho e sha256 do manifesto assinado.
pub(crate) fn download_verified(
    artifact: &PortalArtifact,
    native_tls: bool,
    file_path: &Path,
) -> ResultType<()> {
    let id = build_identity().ok_or_else(|| anyhow!("atualizador desligado"))?;
    check_same_origin(&artifact.url, &id)?;
    let client = http_client(native_tls, DOWNLOAD_TIMEOUT)?;
    super::download::ensure_verified_update_artifact_with_client(
        &client,
        &artifact.url,
        file_path,
        artifact.size,
        &artifact.sha256,
    )
}

// ---------------------------------------------------------------- agenda

fn jitter(max: Duration) -> Duration {
    let secs = max.as_secs();
    if secs == 0 {
        return Duration::ZERO;
    }
    Duration::from_secs(hbb_common::rand::random::<u64>() % (secs + 1))
}

pub(crate) fn initial_delay() -> Duration {
    INITIAL_DELAY + jitter(INITIAL_JITTER)
}

/// 3 h a 5 h.
pub(crate) fn next_interval() -> Duration {
    CHECK_INTERVAL - CHECK_JITTER + jitter(CHECK_JITTER * 2)
}

pub(crate) fn retry_delay() -> Duration {
    RETRY_DELAY + jitter(RETRY_JITTER)
}

// ------------------------------------------------- trava contra laço de reinstalação

#[cfg_attr(not(windows), allow(dead_code))]
fn attempt_marker_path() -> PathBuf {
    hbb_common::config::Config::path(ATTEMPT_MARKER)
}

#[cfg_attr(not(windows), allow(dead_code))]
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn attempt_blocked(marker: Option<&str>, version: &str, now: u64) -> bool {
    let Some(m) = marker else { return false };
    let mut it = m.split_whitespace();
    match (it.next(), it.next().and_then(|t| t.parse::<u64>().ok())) {
        (Some(v), Some(t)) if v == version => now.saturating_sub(t) < SAME_VERSION_BACKOFF_SECS,
        _ => false,
    }
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn recently_attempted(version: &str) -> bool {
    let marker = std::fs::read_to_string(attempt_marker_path()).ok();
    attempt_blocked(marker.as_deref(), version, now_secs())
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn mark_attempt(version: &str) {
    let p = attempt_marker_path();
    if let Err(e) = std::fs::write(&p, format!("{} {}\n", version, now_secs())) {
        log::warn!("[portal-update] não gravei {}: {}", p.display(), e);
    }
}

#[cfg(test)]
mod tests;

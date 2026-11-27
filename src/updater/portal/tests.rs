use super::*;
use hbb_common::sodiumoxide::{self, crypto::sign};

fn id_cliente() -> BuildIdentity {
    BuildIdentity {
        brand: "vancin",
        host: "portal.vancin.com.br",
        variant: "cliente",
        version: (1, 0, 6),
    }
}

fn id_gerencia() -> BuildIdentity {
    BuildIdentity {
        variant: "gerencia",
        ..id_cliente()
    }
}

const WIN: (&str, &str) = ("windows", "x64");
const SHA: &str = "64e2cf54ff0d244984dd675707fb0057ce16c97acfd7e2b49eec84655f9bdaeb";

fn manifest(variant: &str, app_version: &str, file: &str) -> Vec<u8> {
    format!(
        r#"{{"schema":1,"brand":"vancin","variant":"{variant}","version":"1.5.1","app_version":"{app_version}","release":"v{app_version}-vancin-cliente","files":[{{"os":"windows","arch":"x64","file":"{file}","download_name":"Suporte Vancin.exe","size":26025472,"sha256":"{SHA}"}},{{"os":"macos","arch":"arm64","file":"Suporte-Vancin-1.5.1-aarch64.dmg","size":1000,"sha256":"{SHA}"}}]}}"#
    )
    .into_bytes()
}

struct Key {
    pk: [u8; 32],
    sk: sign::SecretKey,
}

fn key() -> Key {
    sodiumoxide::init().unwrap();
    let (pk, sk) = sign::gen_keypair();
    Key { pk: pk.0, sk }
}

fn sign_b64(m: &[u8], k: &Key) -> Vec<u8> {
    let mut msg = SIGNATURE_CONTEXT.to_vec();
    msg.extend_from_slice(m);
    let s = sign::sign_detached(&msg, &k.sk);
    format!("{}\n", STANDARD.encode(s.to_bytes())).into_bytes()
}

fn verify(m: &[u8], s: &[u8], id: &BuildIdentity, k: &Key) -> ResultType<Verdict> {
    verify_manifest_with_keys(m, s, id, WIN, &[k.pk])
}

#[test]
fn assinatura_valida_aceita_e_monta_url_no_portal() {
    let k = key();
    let m = manifest("cliente", "1.0.7", "Suporte-Vancin-1.5.1-x86_64.exe");
    let v = verify(&m, &sign_b64(&m, &k), &id_cliente(), &k).unwrap();
    assert_eq!(
        v,
        Verdict::Update(PortalArtifact {
            app_version: "1.0.7".into(),
            bundle_version: "1.5.1".into(),
            file_name: "Suporte-Vancin-1.5.1-x86_64.exe".into(),
            url: "https://portal.vancin.com.br/downloads/suporte/vancin/Suporte-Vancin-1.5.1-x86_64.exe".into(),
            size: 26025472,
            sha256: SHA.into(),
        })
    );
}

#[test]
fn gerencia_usa_pasta_propria() {
    let k = key();
    let m = manifest("gerencia", "1.0.7", "Suporte-Vancin-1.5.1-x86_64.exe");
    match verify(&m, &sign_b64(&m, &k), &id_gerencia(), &k).unwrap() {
        Verdict::Update(a) => assert!(a
            .url
            .starts_with("https://portal.vancin.com.br/downloads/suporte/vancin-gerencia/")),
        v => panic!("{v:?}"),
    }
}

#[test]
fn assinatura_invalida_recusada() {
    let k = key();
    let m = manifest("cliente", "1.0.7", "Suporte-Vancin-1.5.1-x86_64.exe");
    let s = sign_b64(&m, &k);
    // manifesto alterado depois de assinado
    let mut adulterado = m.clone();
    let pos = adulterado.len() - 3;
    adulterado[pos] ^= 1;
    assert!(verify(&adulterado, &s, &id_cliente(), &k).is_err());
    // assinatura de outra chave
    let outra = key();
    assert!(verify(&m, &sign_b64(&m, &outra), &id_cliente(), &k).is_err());
    // assinatura sem o prefixo de contexto
    let crua = sign::sign_detached(&m, &k.sk);
    let crua = STANDARD.encode(crua.to_bytes()).into_bytes();
    assert!(verify(&m, &crua, &id_cliente(), &k).is_err());
    // lixo / tamanho errado
    assert!(verify(&m, b"AAAA", &id_cliente(), &k).is_err());
    assert!(verify(&m, b"nao e base64 !!", &id_cliente(), &k).is_err());
}

#[test]
fn manifesto_sem_assinatura_recusado() {
    let k = key();
    let m = manifest("cliente", "1.0.7", "Suporte-Vancin-1.5.1-x86_64.exe");
    let e = verify(&m, b"", &id_cliente(), &k).unwrap_err();
    assert!(e.to_string().contains("sem assinatura"), "{e}");
    // o portal responde index.html (200) quando o .sig não existe
    let html = b"<!DOCTYPE html><html><head><title>Portal</title></head></html>";
    assert!(verify(&m, html, &id_cliente(), &k).is_err());
}

#[test]
fn chave_de_producao_nao_aceita_assinatura_de_teste() {
    let k = key();
    let m = manifest("cliente", "1.0.7", "Suporte-Vancin-1.5.1-x86_64.exe");
    assert!(verify_manifest(&m, &sign_b64(&m, &k), &id_cliente(), WIN).is_err());
}

#[test]
fn sem_downgrade_nem_mesma_versao() {
    let k = key();
    for v in ["1.0.6", "1.0.5", "0.9.9"] {
        let m = manifest("cliente", v, "Suporte-Vancin-1.5.1-x86_64.exe");
        assert_eq!(
            verify(&m, &sign_b64(&m, &k), &id_cliente(), &k).unwrap(),
            Verdict::UpToDate,
            "{v}"
        );
    }
    // versão malformada é erro, não "atualizado"
    for v in ["1.0", "1.0.7-beta", "v1.0.7", "1.0.x"] {
        let m = manifest("cliente", v, "Suporte-Vancin-1.5.1-x86_64.exe");
        assert!(
            verify(&m, &sign_b64(&m, &k), &id_cliente(), &k).is_err(),
            "{v}"
        );
    }
    // comparação numérica, não de texto
    let m = manifest("cliente", "1.0.10", "Suporte-Vancin-1.5.1-x86_64.exe");
    assert!(matches!(
        verify(&m, &sign_b64(&m, &k), &id_cliente(), &k).unwrap(),
        Verdict::Update(_)
    ));
}

#[test]
fn variante_errada_recusada() {
    let k = key();
    // cliente recebendo manifesto da gerência
    let m = manifest("gerencia", "1.0.7", "Suporte-Vancin-1.5.1-x86_64.exe");
    assert!(verify(&m, &sign_b64(&m, &k), &id_cliente(), &k).is_err());
    // gerência recebendo manifesto do cliente
    let m = manifest("cliente", "1.0.7", "Suporte-Vancin-1.5.1-x86_64.exe");
    assert!(verify(&m, &sign_b64(&m, &k), &id_gerencia(), &k).is_err());
    // manifesto antigo, sem variante
    let m = br#"{"schema":1,"brand":"vancin","app_version":"1.0.7","files":[]}"#;
    assert!(verify(m, &sign_b64(m, &k), &id_cliente(), &k).is_err());
}

#[test]
fn marca_e_schema_errados_recusados() {
    let k = key();
    let m = String::from_utf8(manifest("cliente", "1.0.7", "a.exe")).unwrap();
    let outra_marca = m.replace(r#""brand":"vancin""#, r#""brand":"talkincloud""#);
    let sem_schema = m.replace(r#""schema":1,"#, "");
    let schema_2 = m.replace(r#""schema":1"#, r#""schema":2"#);
    let sem_app_version = m.replace(r#""app_version":"1.0.7","#, "");
    for bad in [outra_marca, sem_schema, schema_2, sem_app_version] {
        let b = bad.into_bytes();
        assert!(verify(&b, &sign_b64(&b, &k), &id_cliente(), &k).is_err());
    }
}

#[test]
fn host_diferente_recusado() {
    let k = key();
    // campo host assinado, mas de outro domínio
    let m = String::from_utf8(manifest("cliente", "1.0.7", "a.exe"))
        .unwrap()
        .replace(r#""schema":1,"#, r#""schema":1,"host":"evil.example.com","#)
        .into_bytes();
    assert!(verify(&m, &sign_b64(&m, &k), &id_cliente(), &k).is_err());
    // nome de arquivo tentando sair do host/pasta
    for f in [
        "https://evil.example.com/a.exe",
        "//evil.example.com/a.exe",
        "../vancin-gerencia/a.exe",
        "sub/a.exe",
        "a.exe?x=1",
        "a.exe#x",
        "C:a.exe",
        ".a.exe",
        "",
    ] {
        let m = manifest("cliente", "1.0.7", f);
        assert!(
            verify(&m, &sign_b64(&m, &k), &id_cliente(), &k).is_err(),
            "{f}"
        );
    }
    // a URL de download só pode ser do portal da marca, na pasta da variante
    let id = id_cliente();
    assert!(check_same_origin(
        "https://portal.vancin.com.br/downloads/suporte/vancin/a.exe",
        &id
    )
    .is_ok());
    for u in [
        "http://portal.vancin.com.br/downloads/suporte/vancin/a.exe",
        "https://portal.talkincloud.com.br/downloads/suporte/vancin/a.exe",
        "https://portal.vancin.com.br.evil.com/downloads/suporte/vancin/a.exe",
        "https://portal.vancin.com.br:8443/downloads/suporte/vancin/a.exe",
        "https://user@portal.vancin.com.br/downloads/suporte/vancin/a.exe",
        "https://portal.vancin.com.br/downloads/suporte/vancin-gerencia/a.exe",
        "https://portal.vancin.com.br/downloads/suporte/vancin/../vancin-gerencia/a.exe",
        "https://portal.vancin.com.br/downloads/suporte/vancin/a.exe?x",
    ] {
        assert!(check_same_origin(u, &id).is_err(), "{u}");
    }
}

#[test]
fn sha_ou_tamanho_invalidos_no_manifesto_recusados() {
    let k = key();
    let m = String::from_utf8(manifest("cliente", "1.0.7", "a.exe")).unwrap();
    for bad in [
        m.replacen(SHA, "abc", 1),
        m.replacen(SHA, &SHA.to_uppercase(), 1),
        m.replacen("26025472", "0", 1),
        m.replacen("26025472", "999999999999", 1),
    ] {
        let b = bad.into_bytes();
        assert!(verify(&b, &sign_b64(&b, &k), &id_cliente(), &k).is_err());
    }
}

#[test]
fn sha_diferente_no_download_nao_grava_instalador() {
    let dir = std::env::temp_dir().join(format!("portal-update-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let alvo = dir.join("Suporte-Vancin-1.5.1-x86_64.exe");
    let conteudo = b"instalador de teste";
    let sha_certo = crate::update_hash::sha256_reader_hex(&mut &conteudo[..]).unwrap();
    // sha diferente: erro e nada fica no destino
    let r = crate::updater::download::write_verified_update_artifact(
        &alvo,
        &mut &conteudo[..],
        conteudo.len() as u64,
        SHA,
    );
    assert!(r.is_err());
    assert!(!alvo.exists());
    // tamanho diferente: erro
    let r = crate::updater::download::write_verified_update_artifact(
        &alvo,
        &mut &conteudo[..],
        conteudo.len() as u64 + 1,
        &sha_certo,
    );
    assert!(r.is_err());
    assert!(!alvo.exists());
    // sha certo: grava
    crate::updater::download::write_verified_update_artifact(
        &alvo,
        &mut &conteudo[..],
        conteudo.len() as u64,
        &sha_certo,
    )
    .unwrap();
    assert_eq!(std::fs::read(&alvo).unwrap(), conteudo);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn sistema_sem_instalador_no_manifesto() {
    let k = key();
    let m = manifest("cliente", "1.0.7", "a.exe");
    assert!(verify_manifest_with_keys(
        &m,
        &sign_b64(&m, &k),
        &id_cliente(),
        ("linux", "x64"),
        &[k.pk]
    )
    .is_err());
}

/// Assinatura feita pelo `assinar-manifesto-suporte.sh` (OpenSSL + Chaves) com uma chave de
/// teste já apagada: garante que o script e o app usam o mesmo formato.
#[test]
fn interoperabilidade_com_script_openssl() {
    let pk = STANDARD
        .decode("RXdPH3WRbOev4j46DeHgje/LzkULgSq/nzCjLhKaHeo=")
        .unwrap();
    let mut pk32 = [0u8; 32];
    pk32.copy_from_slice(&pk);
    let manifest = b"{\"brand\":\"vancin\",\"variant\":\"cliente\",\"app_version\":\"1.0.6\"}\n";
    let sig = decode_signature(
        b"IUxO70V4DV5Pw6oZPQBkTsu/UsuzR15r9bh3C/XEoHD91k1UhaobTd1Zr7ZBdC4Z0AsuHxe5D1hEDEHu1sLQAw==\n",
    )
    .unwrap();
    sodiumoxide::init().unwrap();
    assert!(signature_ok(manifest, &sig, &[pk32]));
    assert!(!signature_ok(b"{}", &sig, &[pk32]));
    assert!(!signature_ok(manifest, &sig, TRUSTED_KEYS));
}

#[test]
fn identidade_da_compilacao() {
    assert_eq!(
        brand_from_app_name("SuporteVancin"),
        Some(("vancin", "portal.vancin.com.br"))
    );
    assert_eq!(
        brand_from_app_name("SuporteTalkInCloud"),
        Some(("talkincloud", "portal.talkincloud.com.br"))
    );
    assert_eq!(brand_from_app_name("RustDesk"), None);
    assert_eq!(
        id_cliente().base_url(),
        "https://portal.vancin.com.br/downloads/suporte/vancin/"
    );
    assert_eq!(target_for("windows", "x86_64"), Some(("windows", "x64")));
    assert_eq!(target_for("macos", "aarch64"), Some(("macos", "arm64")));
    assert_eq!(target_for("macos", "x86_64"), None);
    assert_eq!(target_for("linux", "x86_64"), Some(("linux", "x64")));
    assert_eq!(parse_version("1.0.10"), Some((1, 0, 10)));
    assert_eq!(parse_version("1.0"), None);
    assert_eq!(parse_version("1.0.6.1"), None);
}

#[test]
fn trava_contra_reinstalar_a_mesma_versao() {
    let now = 1_800_000_000;
    assert!(!attempt_blocked(None, "1.0.7", now));
    assert!(attempt_blocked(Some("1.0.7 1799999000\n"), "1.0.7", now));
    assert!(!attempt_blocked(Some("1.0.7 1700000000\n"), "1.0.7", now));
    assert!(!attempt_blocked(Some("1.0.6 1799999000\n"), "1.0.7", now));
    assert!(!attempt_blocked(Some("lixo"), "1.0.7", now));
}

#[test]
fn intervalos_com_variacao() {
    for _ in 0..200 {
        let i = next_interval().as_secs();
        assert!((3 * 3600..=5 * 3600).contains(&i), "{i}");
        let d = initial_delay().as_secs();
        assert!((30..=210).contains(&d), "{d}");
        let r = retry_delay().as_secs();
        assert!((1800..=2400).contains(&r), "{r}");
    }
}

/// Vetor gerado pelo `publicar-app-suporte.sh` real (modo bancada, chave temporária já
/// apagada do Chaves): o app aceita exatamente o que o script publica.
#[test]
fn manifesto_publicado_pelo_script_aceito() {
    let man = include_bytes!("vetor/manifest.json");
    let sig = include_bytes!("vetor/manifest.json.sig");
    let pk = STANDARD
        .decode(include_str!("vetor/chave-publica-teste.b64").trim())
        .unwrap();
    let mut pk32 = [0u8; 32];
    pk32.copy_from_slice(&pk);
    sodiumoxide::init().unwrap();
    let keys = [pk32];

    match verify_manifest_with_keys(man, sig, &id_cliente(), WIN, &keys).unwrap() {
        Verdict::Update(a) => {
            assert_eq!(a.app_version, "1.0.7");
            assert_eq!(a.file_name, "Suporte-Vancin-1.5.1-x86_64.exe");
            assert_eq!(
                a.url,
                "https://portal.vancin.com.br/downloads/suporte/vancin/Suporte-Vancin-1.5.1-x86_64.exe"
            );
            assert_eq!(a.size, 9);
        }
        Verdict::UpToDate => panic!("1.0.7 > 1.0.6"),
    }
    match verify_manifest_with_keys(man, sig, &id_cliente(), ("macos", "arm64"), &keys).unwrap() {
        Verdict::Update(a) => assert_eq!(a.file_name, "Suporte-Vancin-1.5.1-aarch64.dmg"),
        Verdict::UpToDate => panic!("esperava atualização"),
    }
    // mesma publicação: gerência, já na 1.0.7, chave de produção e Linux (sem .deb) recusam
    assert!(verify_manifest_with_keys(man, sig, &id_gerencia(), WIN, &keys).is_err());
    let atual = BuildIdentity {
        version: (1, 0, 7),
        ..id_cliente()
    };
    assert!(matches!(
        verify_manifest_with_keys(man, sig, &atual, WIN, &keys).unwrap(),
        Verdict::UpToDate
    ));
    assert!(verify_manifest_with_keys(man, sig, &id_cliente(), WIN, TRUSTED_KEYS).is_err());
    assert!(verify_manifest_with_keys(man, sig, &id_cliente(), ("linux", "x64"), &keys).is_err());
}

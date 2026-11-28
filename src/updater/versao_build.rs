//! Decisão da versão do app de suporte no momento do build.
//!
//! Este arquivo **não** usa nada além da `std`: o `build.rs` o inclui com `#[path]` e decide
//! com ele o `SUPORTE_VERSAO` que vai para o binário. Na lib, ele só é compilado nos testes, e
//! os testes provam a regra que o `build.rs` aplica.
//!
//! - **Build normal:** `SUPORTE_VERSAO` explícita (não vazia) vale como veio. Sem ela, a versão
//!   sai da tag (`v1.0.6-vancin-cliente` → `1.0.6`). Sem versão válida (ex.: `nightly`), o
//!   binário fica sem identidade e o atualizador do portal desligado.
//! - **Build de bancada** (feature `bancada-pausas`): a identidade tem de resolver, senão o
//!   build **falha**. Exige marca conhecida em `SUPORTE_APP_NAME`, `SUPORTE_SOMENTE_ENTRADA`
//!   vazio, `0` ou `1` e `SUPORTE_VERSAO` explícita `x.y.z`. A tag é ignorada: com `nightly`
//!   e sem versão explícita, o binário cairia no atualizador antigo (`update_me` sem
//!   `build_identity()`), que a bancada não serve para testar.

/// Marcas aceitas (o mesmo conjunto de `portal::brand_from_app_name`).
pub const MARCAS: [&str; 2] = ["SuporteVancin", "SuporteTalkInCloud"];

/// `x.y.z`, cada parte com 1 a 6 dígitos (a mesma regra de `portal::parse_version`).
pub fn versao_valida(v: &str) -> bool {
    let partes: Vec<&str> = v.split('.').collect();
    partes.len() == 3
        && partes
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|b| b.is_ascii_digit()))
}

/// Versão contida na tag do release, se houver (`v1.0.6-vancin` → `1.0.6`).
pub fn versao_da_tag(tag: &str) -> Option<String> {
    let v = tag.strip_prefix('v').unwrap_or(tag);
    let v = v.split('-').next().unwrap_or("");
    versao_valida(v).then(|| v.to_owned())
}

/// O que o `build.rs` faz com a versão.
#[derive(Debug, PartialEq, Eq)]
pub enum Versao {
    /// `SUPORTE_VERSAO` do ambiente já chega ao `option_env!`; nada a emitir.
    DoAmbiente,
    /// Emitir `cargo:rustc-env=SUPORTE_VERSAO=<valor>` (derivada da tag).
    DaTag(String),
    /// Sem versão: binário sem identidade, atualizador do portal desligado.
    Nenhuma,
}

/// Decide a versão. `Err` = o build tem de falhar (só acontece no modo bancada).
pub fn resolver(
    app_name: Option<&str>,
    explicita: Option<&str>,
    tag: Option<&str>,
    somente_entrada: Option<&str>,
    bancada: bool,
) -> Result<Versao, String> {
    let explicita = explicita.filter(|v| !v.trim().is_empty());
    if !bancada {
        if explicita.is_some() {
            return Ok(Versao::DoAmbiente);
        }
        return Ok(match tag.and_then(versao_da_tag) {
            Some(v) => Versao::DaTag(v),
            None => Versao::Nenhuma,
        });
    }
    match app_name {
        Some(n) if MARCAS.contains(&n) => {}
        outro => {
            return Err(format!(
                "build de bancada sem marca conhecida em SUPORTE_APP_NAME ({outro:?}); \
                 use {MARCAS:?}"
            ))
        }
    }
    if !matches!(somente_entrada, None | Some("") | Some("0") | Some("1")) {
        return Err(format!(
            "build de bancada com SUPORTE_SOMENTE_ENTRADA inválido: {somente_entrada:?}"
        ));
    }
    match explicita {
        Some(v) if versao_valida(v) => Ok(Versao::DoAmbiente),
        Some(v) => Err(format!(
            "build de bancada com SUPORTE_VERSAO inválida: {v:?} (esperado x.y.z)"
        )),
        None => Err(format!(
            "build de bancada exige SUPORTE_VERSAO explícita (x.y.z); a tag {:?} não conta: \
             sem ela o binário sai sem identidade e usa o atualizador antigo",
            tag.unwrap_or("")
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V: &str = "SuporteVancin";
    const T: &str = "SuporteTalkInCloud";

    #[test]
    fn bancada_com_nightly_sem_versao_explicita_e_recusada() {
        for explicita in [None, Some(""), Some("  ")] {
            let e = resolver(Some(V), explicita, Some("nightly"), Some("0"), true).unwrap_err();
            assert!(e.contains("exige SUPORTE_VERSAO"), "{e}");
            assert!(e.contains("nightly"), "{e}");
        }
    }

    #[test]
    fn bancada_ignora_a_tag_mesmo_valida() {
        // Mesmo com uma tag de release, a bancada só aceita a versão explícita.
        let e = resolver(Some(V), None, Some("v1.0.6-vancin"), Some("0"), true).unwrap_err();
        assert!(e.contains("exige SUPORTE_VERSAO"), "{e}");
    }

    #[test]
    fn bancada_com_versao_explicita_valida_passa() {
        for app in [V, T] {
            for se in [None, Some(""), Some("0"), Some("1")] {
                assert_eq!(
                    resolver(Some(app), Some("1.0.6"), Some("nightly"), se, true),
                    Ok(Versao::DoAmbiente)
                );
            }
        }
    }

    #[test]
    fn bancada_recusa_versao_mal_formada() {
        for v in ["1.0", "1.0.6.1", "v1.0.6", "1.0.x", "1.0.6-rc", "1234567.0.0", " 1.0.6"] {
            let e = resolver(Some(V), Some(v), Some("nightly"), Some("0"), true).unwrap_err();
            assert!(e.contains("inválida"), "{v}: {e}");
        }
    }

    #[test]
    fn bancada_recusa_marca_desconhecida_ou_ausente() {
        for app in [None, Some(""), Some("RustDesk"), Some("suportevancin")] {
            let e = resolver(app, Some("1.0.6"), Some("nightly"), Some("0"), true).unwrap_err();
            assert!(e.contains("SUPORTE_APP_NAME"), "{app:?}: {e}");
        }
    }

    #[test]
    fn bancada_recusa_somente_entrada_invalido() {
        let e = resolver(Some(V), Some("1.0.6"), Some("nightly"), Some("sim"), true).unwrap_err();
        assert!(e.contains("SUPORTE_SOMENTE_ENTRADA"), "{e}");
    }

    #[test]
    fn build_normal_mantem_o_comportamento_anterior() {
        // nightly sem versão: sem identidade (atualizador desligado), mas o build não falha.
        assert_eq!(resolver(Some(V), None, Some("nightly"), Some("0"), false), Ok(Versao::Nenhuma));
        assert_eq!(resolver(None, None, None, None, false), Ok(Versao::Nenhuma));
        assert_eq!(
            resolver(Some(V), None, Some("v1.0.6-vancin-cliente"), Some("1"), false),
            Ok(Versao::DaTag("1.0.6".into()))
        );
        assert_eq!(
            resolver(Some(T), Some("1.0.7"), Some("v1.0.6-talkincloud"), None, false),
            Ok(Versao::DoAmbiente)
        );
    }

    #[test]
    fn mesma_regra_de_versao_que_o_portal() {
        // O que o build.rs aceita é exatamente o que build_identity() aceita.
        for v in ["1.0.6", "0.0.0", "999999.1.2", "1.0", "1.0.6.1", "a.b.c", "1..6", "1234567.0.0"] {
            assert_eq!(
                versao_valida(v),
                crate::updater::portal::parse_version(v).is_some(),
                "{v}"
            );
        }
    }

    /// Com a feature de bancada, o próprio binário de teste foi compilado pelo `build.rs` no
    /// modo bancada: se chegou aqui, a identidade resolveu, e o atualizador usado é o novo.
    #[cfg(feature = "bancada-pausas")]
    #[test]
    fn build_de_bancada_tem_identidade() {
        let id = crate::updater::portal::build_identity()
            .expect("build de bancada sem identidade: cairia no atualizador antigo");
        assert_eq!(
            crate::updater::portal::version_string(id.version),
            option_env!("SUPORTE_VERSAO").unwrap()
        );
    }
}

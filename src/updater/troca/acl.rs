//! Proteção das pastas que a troca executa ou troca como SYSTEM (achado 1 da revisão R2).
//!
//! Regra: só atualiza quando a instalação e a pasta-mãe só podem ser gravadas por SYSTEM,
//! Administradores e TrustedInstaller (`Regra::Estrita`), e quando nenhum ancestral permite a
//! outro usuário apagar, renomear ou trocar permissões de um item do caminho
//! (`Regra::Caminho`). Reparse points (junções, links, pontos de montagem) são recusados em
//! qualquer nível. As pastas criadas pela troca (`.novo` e a pasta da recuperação) nascem com
//! DACL explícita e protegida (`SDDL_PROTEGIDA`) e são conferidas de novo antes de executar e
//! antes de trocar.
//!
//! Por que duas regras: `C:\` e `C:\ProgramData` deixam usuários comuns *criar* pastas
//! (FILE_ADD_SUBDIRECTORY) por padrão. A regra estrita nesses níveis recusaria toda máquina.
//! Criar itens novos no ancestral não troca o que já está no caminho. Para isso seria preciso
//! DELETE no item, FILE_DELETE_CHILD no pai, ou WRITE_DAC/WRITE_OWNER para se dar esses
//! direitos, e é isso que a regra de caminho proíbe.
//!
//! A decisão (`avaliar`, `conferir_cadeia`) é portátil e testada em qualquer sistema. Só a
//! leitura do descritor e a criação da pasta são específicas do Windows.

use std::{
    io,
    path::{Component, Path},
};

pub(crate) const SID_SYSTEM: &str = "S-1-5-18";
pub(crate) const SID_ADMINISTRADORES: &str = "S-1-5-32-544";
pub(crate) const SID_TRUSTED_INSTALLER: &str =
    "S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464";

/// DACL das pastas criadas pela troca: `P` = protegida (não herda da pasta-mãe); SYSTEM e
/// Administradores com controle total; Usuários só leitura e execução (0x1200a9); `OICI` =
/// vale também para os arquivos e subpastas criados dentro.
pub(crate) const SDDL_PROTEGIDA: &str =
    "D:P(A;OICI;FA;;;SY)(A;OICI;FA;;;BA)(A;OICI;0x1200a9;;;BU)";

pub(crate) const INHERIT_ONLY_ACE: u8 = 0x08;

const FILE_WRITE_DATA: u32 = 0x0002; // = FILE_ADD_FILE em pasta
const FILE_APPEND_DATA: u32 = 0x0004; // = FILE_ADD_SUBDIRECTORY em pasta
const FILE_WRITE_EA: u32 = 0x0010;
const FILE_DELETE_CHILD: u32 = 0x0040;
const FILE_WRITE_ATTRIBUTES: u32 = 0x0100;
const DELETE: u32 = 0x0001_0000;
const WRITE_DAC: u32 = 0x0004_0000;
const WRITE_OWNER: u32 = 0x0008_0000;
const MAXIMUM_ALLOWED: u32 = 0x0200_0000;
const GENERIC_ALL: u32 = 0x1000_0000;
const GENERIC_WRITE: u32 = 0x4000_0000;

/// Qualquer gravação (regra estrita).
pub(crate) const MASCARA_ESCRITA: u32 = FILE_WRITE_DATA
    | FILE_APPEND_DATA
    | FILE_WRITE_EA
    | FILE_DELETE_CHILD
    | FILE_WRITE_ATTRIBUTES
    | DELETE
    | WRITE_DAC
    | WRITE_OWNER
    | MAXIMUM_ALLOWED
    | GENERIC_ALL
    | GENERIC_WRITE;

/// Direitos que permitem tirar/trocar um item do caminho (regra de caminho).
pub(crate) const MASCARA_TROCA_DE_ITEM: u32 =
    FILE_DELETE_CHILD | DELETE | WRITE_DAC | WRITE_OWNER | MAXIMUM_ALLOWED | GENERIC_ALL;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Regra {
    Estrita,
    Caminho,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Ace {
    /// `false` = ACE de negação (só reduz acesso; ignorada).
    pub permitir: bool,
    pub sid: String,
    pub mascara: u32,
    pub flags: u8,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Descritor {
    pub dono: String,
    /// `None` = DACL nula (acesso total para todos).
    pub dacl: Option<Vec<Ace>>,
    pub reparse: bool,
}

pub(crate) fn confiavel(sid: &str) -> bool {
    matches!(sid, SID_SYSTEM | SID_ADMINISTRADORES | SID_TRUSTED_INSTALLER)
}

pub(crate) fn avaliar(d: &Descritor, regra: Regra) -> Result<(), String> {
    if d.reparse {
        return Err("é reparse point (junção, link ou ponto de montagem)".to_owned());
    }
    if !confiavel(&d.dono) {
        // o dono sempre pode reescrever a DACL
        return Err(format!(
            "dono {} não é SYSTEM, Administradores nem TrustedInstaller",
            d.dono
        ));
    }
    let aces = d
        .dacl
        .as_ref()
        .ok_or_else(|| "DACL nula (qualquer um grava)".to_owned())?;
    let proibido = match regra {
        Regra::Estrita => MASCARA_ESCRITA,
        Regra::Caminho => MASCARA_TROCA_DE_ITEM,
    };
    for a in aces {
        if !a.permitir || a.flags & INHERIT_ONLY_ACE != 0 || confiavel(&a.sid) {
            continue;
        }
        if a.mascara & proibido != 0 {
            return Err(format!(
                "{} tem direito de gravação (máscara {:#x}, regra {:?})",
                a.sid, a.mascara, regra
            ));
        }
    }
    Ok(())
}

/// Confere `alvo` e todos os ancestrais. Os `estritos` primeiros níveis (alvo, pasta-mãe…)
/// usam a regra estrita; o resto usa a de caminho. Caminho relativo ou com `.`/`..` é
/// recusado.
pub(crate) fn conferir_cadeia(
    alvo: &Path,
    estritos: usize,
    ler: &mut dyn FnMut(&Path) -> io::Result<Descritor>,
) -> Result<(), String> {
    if !alvo.has_root()
        || alvo
            .components()
            .any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return Err(format!("caminho não absoluto/canônico: {alvo:?}"));
    }
    for (i, p) in alvo.ancestors().enumerate() {
        if p.as_os_str().is_empty() {
            continue;
        }
        let regra = if i < estritos {
            Regra::Estrita
        } else {
            Regra::Caminho
        };
        let d = ler(p).map_err(|e| format!("{p:?}: não consegui ler a segurança: {e}"))?;
        avaliar(&d, regra).map_err(|e| format!("{p:?}: {e}"))?;
    }
    Ok(())
}

#[cfg(windows)]
mod win {
    use super::{Ace, Descritor, SDDL_PROTEGIDA};
    use std::{
        ffi::c_void,
        io,
        os::windows::{ffi::OsStrExt, fs::MetadataExt},
        path::Path,
    };
    use windows::{
        core::{PCWSTR, PWSTR},
        Win32::{
            Foundation::{LocalFree, HLOCAL},
            Security::{
                Authorization::{
                    ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                    GetNamedSecurityInfoW, SDDL_REVISION_1, SE_FILE_OBJECT,
                },
                GetAce, ACCESS_ALLOWED_ACE, ACE_HEADER, ACL, DACL_SECURITY_INFORMATION,
                OWNER_SECURITY_INFORMATION, PSECURITY_DESCRIPTOR, PSID, SECURITY_ATTRIBUTES,
            },
            Storage::FileSystem::CreateDirectoryW,
        },
    };

    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;

    fn largo(p: &Path) -> Vec<u16> {
        p.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    struct Libera(*mut c_void);
    impl Drop for Libera {
        fn drop(&mut self) {
            if !self.0.is_null() {
                unsafe {
                    let _ = LocalFree(Some(HLOCAL(self.0)));
                }
            }
        }
    }

    fn sid_texto(sid: PSID) -> io::Result<String> {
        let mut s = PWSTR::null();
        unsafe { ConvertSidToStringSidW(sid, &mut s) }.map_err(io::Error::from)?;
        let _g = Libera(s.0 as *mut c_void);
        unsafe { s.to_string() }.map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
    }

    /// Dono, DACL e se é reparse point (sem seguir o link).
    pub(crate) fn ler_descritor(p: &Path) -> io::Result<Descritor> {
        let reparse =
            std::fs::symlink_metadata(p)?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0;
        let nome = largo(p);
        let mut dono = PSID::default();
        let mut dacl: *mut ACL = std::ptr::null_mut();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        let r = unsafe {
            GetNamedSecurityInfoW(
                PCWSTR(nome.as_ptr()),
                SE_FILE_OBJECT,
                OWNER_SECURITY_INFORMATION | DACL_SECURITY_INFORMATION,
                Some(&mut dono),
                None,
                Some(&mut dacl),
                None,
                &mut sd,
            )
        };
        if r.is_err() {
            return Err(io::Error::from_raw_os_error(r.0 as i32));
        }
        let _g = Libera(sd.0);
        let dono = sid_texto(dono)?;
        let dacl = if dacl.is_null() {
            None
        } else {
            let n = unsafe { (*dacl).AceCount } as u32;
            let mut v = Vec::with_capacity(n as usize);
            for i in 0..n {
                let mut pace: *mut c_void = std::ptr::null_mut();
                unsafe { GetAce(dacl, i, &mut pace) }.map_err(io::Error::from)?;
                let h = unsafe { &*(pace as *const ACE_HEADER) };
                // 0/9 = permitir (simples/callback), 1/10 = negar: Header, Mask, SidStart
                let permitir = match h.AceType {
                    0 | 9 => true,
                    1 | 10 => false,
                    t => {
                        // tipos de objeto não existem em arquivo; desconhecido = recusar
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            format!("ACE de tipo {t} não suportado"),
                        ));
                    }
                };
                let a = unsafe { &*(pace as *const ACCESS_ALLOWED_ACE) };
                let sid = PSID(&a.SidStart as *const u32 as *mut c_void);
                v.push(Ace {
                    permitir,
                    sid: sid_texto(sid)?,
                    mascara: a.Mask,
                    flags: h.AceFlags,
                });
            }
            Some(v)
        };
        Ok(Descritor {
            dono,
            dacl,
            reparse,
        })
    }

    /// Cria a pasta (falha se já existir) já com `SDDL_PROTEGIDA`.
    pub(crate) fn criar_pasta_protegida(p: &Path) -> io::Result<()> {
        let sddl: Vec<u16> = SDDL_PROTEGIDA.encode_utf16().chain(Some(0)).collect();
        let mut sd = PSECURITY_DESCRIPTOR::default();
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(sddl.as_ptr()),
                SDDL_REVISION_1,
                &mut sd,
                None,
            )
        }
        .map_err(io::Error::from)?;
        let _g = Libera(sd.0);
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: sd.0,
            bInheritHandle: false.into(),
        };
        let nome = largo(p);
        unsafe { CreateDirectoryW(PCWSTR(nome.as_ptr()), Some(&sa)) }.map_err(io::Error::from)
    }
}

#[cfg(windows)]
pub(crate) use win::{criar_pasta_protegida, ler_descritor};

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashMap, path::PathBuf};

    const USUARIOS: &str = "S-1-5-32-545";
    const AUTENTICADOS: &str = "S-1-5-11";
    const CRIADOR: &str = "S-1-3-0";
    const FULANO: &str = "S-1-5-21-1-2-3-1001";

    fn ace(sid: &str, mascara: u32, flags: u8) -> Ace {
        Ace {
            permitir: true,
            sid: sid.to_owned(),
            mascara,
            flags,
        }
    }

    fn d(dono: &str, aces: Vec<Ace>) -> Descritor {
        Descritor {
            dono: dono.to_owned(),
            dacl: Some(aces),
            reparse: false,
        }
    }

    // Padrões do Windows 10/11 (icacls), por SID.
    fn raiz_c() -> Descritor {
        d(
            SID_SYSTEM,
            vec![
                ace(SID_ADMINISTRADORES, 0x1f01ff, 0x03),
                ace(SID_SYSTEM, 0x1f01ff, 0x03),
                ace(USUARIOS, 0x1200a9, 0x03),
                ace(AUTENTICADOS, 0x0004, 0x00), // criar pastas na raiz
                ace(AUTENTICADOS, 0xe001_0000, 0x0b), // Modify, só herança
            ],
        )
    }
    fn program_files() -> Descritor {
        d(
            SID_TRUSTED_INSTALLER,
            vec![
                ace(SID_TRUSTED_INSTALLER, 0x1f01ff, 0x02),
                ace(SID_SYSTEM, 0x1301bf, 0x00),
                ace(SID_ADMINISTRADORES, 0x1301bf, 0x00),
                ace(USUARIOS, 0x1200a9, 0x00),
                ace(CRIADOR, 0x1000_0000, 0x0b),
                ace("S-1-15-2-1", 0x1200a9, 0x00),
            ],
        )
    }
    fn instalada_herdada() -> Descritor {
        d(
            SID_ADMINISTRADORES,
            vec![
                ace(SID_TRUSTED_INSTALLER, 0x1f01ff, 0x10),
                ace(SID_SYSTEM, 0x1301bf, 0x10),
                ace(SID_ADMINISTRADORES, 0x1301bf, 0x10),
                ace(USUARIOS, 0x1200a9, 0x10),
            ],
        )
    }
    fn protegida() -> Descritor {
        // o que SDDL_PROTEGIDA produz
        d(
            SID_ADMINISTRADORES,
            vec![
                ace(SID_SYSTEM, 0x1f01ff, 0x03),
                ace(SID_ADMINISTRADORES, 0x1f01ff, 0x03),
                ace(USUARIOS, 0x1200a9, 0x03),
            ],
        )
    }
    fn program_data() -> Descritor {
        d(
            SID_SYSTEM,
            vec![
                ace(SID_SYSTEM, 0x1f01ff, 0x03),
                ace(SID_ADMINISTRADORES, 0x1f01ff, 0x03),
                ace(CRIADOR, 0x1000_0000, 0x0b),
                ace(USUARIOS, 0x1200a9, 0x03),
                ace(USUARIOS, 0x0006, 0x02), // criar arquivos e pastas
            ],
        )
    }
    /// `C:\Ferramentas` criada por um usuário: herda Modify de Autenticados da raiz.
    fn pasta_do_usuario() -> Descritor {
        d(
            FULANO,
            vec![
                ace(SID_ADMINISTRADORES, 0x1f01ff, 0x13),
                ace(SID_SYSTEM, 0x1f01ff, 0x13),
                ace(USUARIOS, 0x1200a9, 0x13),
                ace(AUTENTICADOS, 0x1301bf, 0x10),
            ],
        )
    }

    fn mapa(itens: Vec<(&str, Descritor)>) -> impl FnMut(&Path) -> io::Result<Descritor> {
        let m: HashMap<PathBuf, Descritor> =
            itens.into_iter().map(|(p, d)| (PathBuf::from(p), d)).collect();
        move |p: &Path| {
            m.get(p)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "sem descritor"))
        }
    }

    #[test]
    fn padrao_do_windows_passa() {
        let mut ler = mapa(vec![
            ("/", raiz_c()),
            ("/Program Files", program_files()),
            ("/Program Files/SuporteVancin", instalada_herdada()),
            ("/Program Files/SuporteVancin.novo", protegida()),
            ("/ProgramData", program_data()),
            ("/ProgramData/SuporteVancin", program_data_filha()),
            ("/ProgramData/SuporteVancin/atualizacao", protegida()),
        ]);
        conferir_cadeia(Path::new("/Program Files/SuporteVancin"), 2, &mut ler).unwrap();
        conferir_cadeia(Path::new("/Program Files/SuporteVancin.novo"), 2, &mut ler).unwrap();
        conferir_cadeia(Path::new("/ProgramData/SuporteVancin/atualizacao"), 1, &mut ler)
            .unwrap();
    }

    /// `ProgramData\<App>` criada pelo serviço: herda "Usuários criam arquivos/pastas".
    fn program_data_filha() -> Descritor {
        d(
            SID_ADMINISTRADORES,
            vec![
                ace(SID_SYSTEM, 0x1f01ff, 0x13),
                ace(SID_ADMINISTRADORES, 0x1f01ff, 0x13),
                ace(USUARIOS, 0x1200a9, 0x13),
                ace(USUARIOS, 0x0006, 0x12),
            ],
        )
    }

    #[test]
    fn instalacao_em_pasta_gravavel_e_recusada() {
        let mut ler = mapa(vec![
            ("/", raiz_c()),
            ("/Ferramentas", pasta_do_usuario()),
            ("/Ferramentas/SuporteVancin", instalada_herdada()),
        ]);
        let e = conferir_cadeia(Path::new("/Ferramentas/SuporteVancin"), 2, &mut ler).unwrap_err();
        assert!(e.contains("/Ferramentas") && e.contains("dono"), "{e}");
        // mesmo com dono confiável, Autenticados com Modify na mãe recusa
        let mut mae = pasta_do_usuario();
        mae.dono = SID_ADMINISTRADORES.to_owned();
        let mut ler = mapa(vec![
            ("/", raiz_c()),
            ("/Ferramentas", mae),
            ("/Ferramentas/SuporteVancin", instalada_herdada()),
        ]);
        let e = conferir_cadeia(Path::new("/Ferramentas/SuporteVancin"), 2, &mut ler).unwrap_err();
        assert!(e.contains(AUTENTICADOS), "{e}");
    }

    #[test]
    fn raiz_estrita_recusaria_toda_maquina_por_isso_caminho() {
        assert!(avaliar(&raiz_c(), Regra::Estrita).is_err());
        assert!(avaliar(&raiz_c(), Regra::Caminho).is_ok());
        assert!(avaliar(&program_data(), Regra::Estrita).is_err());
        assert!(avaliar(&program_data(), Regra::Caminho).is_ok());
    }

    #[test]
    fn ancestral_que_deixa_apagar_ou_trocar_dacl_e_recusado() {
        for m in [FILE_DELETE_CHILD, DELETE, WRITE_DAC, WRITE_OWNER, GENERIC_ALL] {
            let mut r = raiz_c();
            r.dacl.as_mut().unwrap().push(ace(USUARIOS, m, 0));
            assert!(avaliar(&r, Regra::Caminho).is_err(), "{m:#x}");
        }
    }

    #[test]
    fn estrita_recusa_qualquer_gravacao_de_terceiros() {
        for m in [
            FILE_WRITE_DATA,
            FILE_APPEND_DATA,
            FILE_WRITE_EA,
            FILE_WRITE_ATTRIBUTES,
            GENERIC_WRITE,
            MAXIMUM_ALLOWED,
        ] {
            let mut p = protegida();
            p.dacl.as_mut().unwrap().push(ace(FULANO, m, 0));
            assert!(avaliar(&p, Regra::Estrita).is_err(), "{m:#x}");
        }
        // negação e ACE só de herança não contam
        let mut p = protegida();
        p.dacl.as_mut().unwrap().push(Ace {
            permitir: false,
            ..ace(FULANO, 0x1f01ff, 0)
        });
        p.dacl.as_mut().unwrap().push(ace(CRIADOR, 0x1f01ff, INHERIT_ONLY_ACE | 0x03));
        assert!(avaliar(&p, Regra::Estrita).is_ok());
    }

    #[test]
    fn reparse_dacl_nula_e_caminho_relativo() {
        let mut p = protegida();
        p.reparse = true;
        assert!(avaliar(&p, Regra::Caminho).unwrap_err().contains("reparse"));
        let mut p = protegida();
        p.dacl = None;
        assert!(avaliar(&p, Regra::Caminho).unwrap_err().contains("nula"));
        let mut ler = mapa(vec![]);
        assert!(conferir_cadeia(Path::new("rel/x"), 1, &mut ler).is_err());
        assert!(conferir_cadeia(Path::new("/a/../b"), 1, &mut ler).is_err());
        // descritor ilegível = recusa
        assert!(conferir_cadeia(Path::new("/x"), 1, &mut ler).is_err());
    }

    #[test]
    fn junção_no_meio_do_caminho_e_recusada() {
        let mut j = program_files();
        j.reparse = true;
        let mut ler = mapa(vec![
            ("/", raiz_c()),
            ("/Program Files", j),
            ("/Program Files/SuporteVancin", instalada_herdada()),
        ]);
        let e = conferir_cadeia(Path::new("/Program Files/SuporteVancin"), 2, &mut ler).unwrap_err();
        assert!(e.contains("reparse"), "{e}");
    }

    #[test]
    fn sddl_protegida_nao_da_escrita_a_usuarios() {
        assert!(SDDL_PROTEGIDA.starts_with("D:P("));
        assert!(SDDL_PROTEGIDA.contains("(A;OICI;0x1200a9;;;BU)"));
        assert_eq!(0x1200a9 & MASCARA_ESCRITA, 0);
        assert_eq!(SDDL_PROTEGIDA.matches("(A;").count(), 3);
    }
}

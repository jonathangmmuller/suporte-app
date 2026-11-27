//! Bancada da troca com falhas injetadas (roda em qualquer sistema: pastas reais em temp,
//! serviço simulado). O serviço "sobe" conforme o conteúdo do EXE que está em `atual`:
//! `BOM…` sobe e fica; `CAI…` sobe e cai em seguida; qualquer outra coisa não sobe.

use super::*;
use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    sync::atomic::{AtomicU32, Ordering},
};

const EXE: &str = "SuporteVancin.exe";
const T0: Tempos = Tempos {
    passo: Duration::ZERO,
    parar: 5,
    subir: 5,
    estavel: 5,
    renomear: 3,
};

fn tmp() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    let d = std::env::temp_dir().join(format!(
        "suporte-troca-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

fn escrever(p: &Path, conteudo: &str) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, conteudo).unwrap();
}

fn ler(p: &Path) -> String {
    fs::read_to_string(p).unwrap_or_else(|_| "<ausente>".to_owned())
}

/// Cenário: instalação atual 1.0.6 (com arquivos extras) e versão nova 1.0.7 extraída.
struct Cenario {
    raiz: PathBuf,
    c: Caminhos,
    origem: PathBuf,
    broker: PathBuf,
}

impl Cenario {
    fn novo(exe_novo: &str) -> Self {
        let raiz = tmp();
        let c = Caminhos::de(&raiz.join("Program Files").join("SuporteVancin"));
        escrever(&c.atual.join(EXE), "BOM 1.0.6");
        escrever(&c.atual.join("librustdesk.dll"), "dll 1.0.6");
        escrever(&c.atual.join("custom.txt"), "config embutida");
        escrever(&c.atual.join("meta.toml"), "msi");
        escrever(&c.atual.join("drivers").join("impressora.inf"), "driver");
        let origem = raiz.join("Temp").join("extraido");
        escrever(&origem.join("rustdesk.exe"), exe_novo);
        escrever(&origem.join("librustdesk.dll"), "dll 1.0.7");
        escrever(&origem.join("data").join("app.so"), "flutter 1.0.7");
        let broker = raiz.join("System32").join("RuntimeBroker.exe");
        escrever(&broker, "broker");
        Self {
            raiz,
            c,
            origem,
            broker,
        }
    }

    fn rodar(
        &self,
        copiar: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
        validar: &mut dyn FnMut(&Path) -> ResultType<()>,
        s: &mut Falso,
    ) -> Resultado {
        let extras = vec![(self.broker.clone(), "RuntimeBroker_rustdesk.exe".to_owned())];
        let p = Preparo {
            origem: &self.origem,
            exe_origem: "rustdesk.exe",
            exe_destino: EXE,
            extras: &extras,
            remover: &["meta.toml"],
        };
        let eventos = s.eventos.clone();
        atualizar(
            &self.c,
            &p,
            copiar,
            validar,
            &mut || {
                eventos.borrow_mut().push("guarda".to_owned());
                Ok(())
            },
            s,
            true,
            &T0,
        )
    }

    /// A instalação atual continua sendo a 1.0.6, íntegra.
    fn anterior_intacta_no_lugar(&self) {
        assert_eq!(ler(&self.c.atual.join(EXE)), "BOM 1.0.6");
        assert_eq!(ler(&self.c.atual.join("librustdesk.dll")), "dll 1.0.6");
        assert_eq!(ler(&self.c.atual.join("custom.txt")), "config embutida");
        assert_eq!(ler(&self.c.atual.join("meta.toml")), "msi");
        assert_eq!(ler(&self.c.atual.join("drivers").join("impressora.inf")), "driver");
    }
}

impl Drop for Cenario {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.raiz);
    }
}

struct Falso {
    atual: PathBuf,
    estado: Servico,
    consultas_ate_cair: Option<u32>,
    /// pasta de origem → quantas vezes ainda falhar o rename
    falhas_renomear: HashMap<PathBuf, u32>,
    eventos: Rc<RefCell<Vec<String>>>,
}

impl Falso {
    fn novo(c: &Caminhos) -> Self {
        Self {
            atual: c.atual.clone(),
            estado: Servico::Rodando,
            consultas_ate_cair: None,
            falhas_renomear: HashMap::new(),
            eventos: Rc::new(RefCell::new(Vec::new())),
        }
    }
    fn ev(&self, e: impl Into<String>) {
        self.eventos.borrow_mut().push(e.into());
    }
    fn eventos(&self) -> Vec<String> {
        self.eventos.borrow().clone()
    }
}

impl Sistema for Falso {
    fn servico(&mut self) -> Servico {
        if let Some(n) = self.consultas_ate_cair.as_mut() {
            if *n == 0 {
                self.estado = Servico::Parado;
                self.consultas_ate_cair = None;
            } else {
                *n -= 1;
            }
        }
        self.estado
    }
    fn parar_servico(&mut self) {
        self.ev("parar");
        self.estado = Servico::Parado;
        self.consultas_ate_cair = None;
    }
    fn iniciar_servico(&mut self) {
        let exe = ler(&self.atual.join(EXE));
        self.ev(format!("iniciar:{exe}"));
        if self.estado == Servico::Rodando {
            return;
        }
        if exe.starts_with("BOM") {
            self.estado = Servico::Rodando;
        } else if exe.starts_with("CAI") {
            self.estado = Servico::Rodando;
            self.consultas_ate_cair = Some(2);
        }
    }
    fn encerrar_processos(&mut self) {
        self.ev("taskkill");
    }
    fn renomear(&mut self, de: &Path, para: &Path) -> io::Result<()> {
        if let Some(n) = self.falhas_renomear.get_mut(de) {
            if *n > 0 {
                *n -= 1;
                self.ev(format!("ren-falhou:{}", de.file_name().unwrap().to_string_lossy()));
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "em uso"));
            }
        }
        self.ev(format!(
            "ren:{}>{}",
            de.file_name().unwrap().to_string_lossy(),
            para.file_name().unwrap().to_string_lossy()
        ));
        fs::rename(de, para)
    }
    fn esperar(&mut self, _d: Duration) {}
}

fn validar_ok(p: &Path) -> ResultType<()> {
    if ler(p).starts_with("BOM") || ler(p).starts_with("CAI") || ler(p).starts_with("RUIM") {
        Ok(())
    } else {
        bail!("EXE não carrega")
    }
}

/// Nenhum `iniciar` entre o `parar` e a nova instalação estar no lugar (não sobe no meio).
fn sem_partida_no_meio(ev: &[String]) {
    let parar = ev.iter().position(|e| e == "parar").expect("parou");
    let no_lugar = ev
        .iter()
        .position(|e| e == "ren:SuporteVancin.novo>SuporteVancin")
        .unwrap_or(ev.len());
    assert!(
        !ev[parar..no_lugar].iter().any(|e| e.starts_with("iniciar")),
        "serviço iniciado no meio da troca: {ev:?}"
    );
}

#[test]
fn preparar_monta_novo_sem_tocar_a_atual() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert_eq!(r, Resultado::Atualizado);
    let c = &k.c;
    // nova no lugar, extras preservados, meta.toml fora, broker copiado
    assert_eq!(ler(&c.atual.join(EXE)), "BOM 1.0.7");
    assert_eq!(ler(&c.atual.join("librustdesk.dll")), "dll 1.0.7");
    assert_eq!(ler(&c.atual.join("data").join("app.so")), "flutter 1.0.7");
    assert_eq!(ler(&c.atual.join("custom.txt")), "config embutida");
    assert_eq!(ler(&c.atual.join("drivers").join("impressora.inf")), "driver");
    assert_eq!(ler(&c.atual.join("RuntimeBroker_rustdesk.exe")), "broker");
    assert!(!c.atual.join("meta.toml").exists());
    assert!(!c.atual.join("rustdesk.exe").exists());
    // anterior íntegra guardada
    assert_eq!(ler(&c.anterior.join(EXE)), "BOM 1.0.6");
    assert_eq!(ler(&c.anterior.join("meta.toml")), "msi");
    assert!(!c.novo.exists());
    assert_eq!(s.estado, Servico::Rodando);
    let ev = s.eventos();
    sem_partida_no_meio(&ev);
    // a guarda é montada antes de parar
    assert!(ev.iter().position(|e| e == "guarda") < ev.iter().position(|e| e == "parar"));
}

#[test]
fn copia_com_erro_no_meio_nao_para_o_servico() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    let mut n = 0;
    let mut copiar = |de: &Path, para: &Path| {
        n += 1;
        if n == 3 {
            return Err(io::Error::new(io::ErrorKind::Other, "disco cheio"));
        }
        copiar_arquivo(de, para)
    };
    let r = k.rodar(&mut copiar, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::NaoTrocado(ref m) if m.contains("disco cheio")), "{r:?}");
    assert!(s.eventos().is_empty(), "serviço tocado: {:?}", s.eventos());
    assert_eq!(s.estado, Servico::Rodando);
    assert!(!k.c.novo.exists(), "sobrou .novo");
    k.anterior_intacta_no_lugar();
}

#[test]
fn copia_parcial_silenciosa_e_detectada_pelo_sha256() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    // copia só metade do librustdesk.dll novo e "dá certo"
    let mut copiar = |de: &Path, para: &Path| {
        if de.ends_with("extraido/librustdesk.dll") {
            let b = fs::read(de)?;
            return fs::write(para, &b[..b.len() / 2]);
        }
        copiar_arquivo(de, para)
    };
    let r = k.rodar(&mut copiar, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::NaoTrocado(ref m) if m.contains("cópia parcial")), "{r:?}");
    assert!(s.eventos().is_empty());
    assert!(!k.c.novo.exists());
    k.anterior_intacta_no_lugar();
}

#[test]
fn exe_invalido_nao_para_o_servico() {
    let k = Cenario::novo("MZ lixo truncado");
    let mut s = Falso::novo(&k.c);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::NaoTrocado(ref m) if m.contains("EXE não carrega")), "{r:?}");
    assert!(s.eventos().is_empty());
    assert!(!k.c.novo.exists());
    k.anterior_intacta_no_lugar();
}

#[test]
fn falha_ao_montar_guarda_nao_para_o_servico() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    let extras: Vec<(PathBuf, String)> = vec![];
    let p = Preparo {
        origem: &k.origem,
        exe_origem: "rustdesk.exe",
        exe_destino: EXE,
        extras: &extras,
        remover: &[],
    };
    let r = atualizar(
        &k.c,
        &p,
        &mut copiar_arquivo,
        &mut validar_ok,
        &mut || bail!("schtasks indisponível"),
        &mut s,
        true,
        &T0,
    );
    assert!(matches!(r, Resultado::NaoTrocado(ref m) if m.contains("schtasks")), "{r:?}");
    assert!(s.eventos().is_empty());
    assert!(!k.c.novo.exists());
    k.anterior_intacta_no_lugar();
}

#[test]
fn servico_que_nao_sobe_volta_a_anterior() {
    let k = Cenario::novo("RUIM 1.0.7");
    let mut s = Falso::novo(&k.c);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::Revertido(_)), "{r:?}");
    k.anterior_intacta_no_lugar();
    assert_eq!(s.estado, Servico::Rodando);
    assert_eq!(ler(&k.c.falhou.join(EXE)), "RUIM 1.0.7");
    let ev = s.eventos();
    sem_partida_no_meio(&ev);
    assert_eq!(ev.last().map(String::as_str), Some("iniciar:BOM 1.0.6"), "{ev:?}");
}

#[test]
fn servico_que_sobe_e_cai_volta_a_anterior() {
    let k = Cenario::novo("CAI 1.0.7");
    let mut s = Falso::novo(&k.c);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::Revertido(_)), "{r:?}");
    k.anterior_intacta_no_lugar();
    assert_eq!(s.estado, Servico::Rodando);
}

#[test]
fn pasta_atual_travada_nao_troca_e_religa() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    s.falhas_renomear.insert(k.c.atual.clone(), 99);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::NaoTrocado(_)), "{r:?}");
    k.anterior_intacta_no_lugar();
    assert_eq!(s.estado, Servico::Rodando);
}

#[test]
fn pasta_atual_travada_por_instantes_troca() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    s.falhas_renomear.insert(k.c.atual.clone(), 2);
    assert_eq!(
        k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s),
        Resultado::Atualizado
    );
    assert_eq!(ler(&k.c.atual.join(EXE)), "BOM 1.0.7");
}

#[test]
fn nova_que_nao_entra_no_lugar_devolve_a_anterior() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    s.falhas_renomear.insert(k.c.novo.clone(), 99);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::NaoTrocado(_)), "{r:?}");
    k.anterior_intacta_no_lugar();
    assert_eq!(s.estado, Servico::Rodando);
    assert!(!k.c.anterior.exists());
}

#[test]
fn anterior_que_nao_volta_e_critico_e_nao_deixa_pasta_vazia() {
    let k = Cenario::novo("RUIM 1.0.7");
    let mut s = Falso::novo(&k.c);
    s.falhas_renomear.insert(k.c.anterior.clone(), 999);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::Critico(_)), "{r:?}");
    assert!(!r.consistente());
    // não deixa a instalação sem pasta: a nova voltou ao lugar e a anterior está guardada
    assert!(k.c.atual.join(EXE).is_file());
    assert_eq!(ler(&k.c.anterior.join(EXE)), "BOM 1.0.6");
    // a guarda (recuperar) resolve depois que o bloqueio some
    s.falhas_renomear.clear();
    assert!(recuperar(&k.c, EXE, &mut s, &T0));
    k.anterior_intacta_no_lugar();
    assert_eq!(s.estado, Servico::Rodando);
}

/// Interrupções (processo morto/queda de energia) em cada ponto: a recuperação da guarda
/// ou da tarefa "ao iniciar" sempre deixa a anterior no lugar e o serviço rodando.
#[test]
fn recuperacao_em_cada_ponto_de_interrupcao() {
    // 0: antes de renomear; 1: depois de atual→anterior; 2: depois de novo→atual (nova ruim)
    for ponto in 0..3 {
        let k = Cenario::novo("RUIM 1.0.7");
        let extras: Vec<(PathBuf, String)> = vec![];
        let p = Preparo {
            origem: &k.origem,
            exe_origem: "rustdesk.exe",
            exe_destino: EXE,
            extras: &extras,
            remover: &[],
        };
        preparar(&k.c, &p, &mut copiar_arquivo).unwrap();
        fs::create_dir_all(&k.c.troca).unwrap();
        if ponto >= 1 {
            fs::rename(&k.c.atual, &k.c.anterior).unwrap();
        }
        if ponto >= 2 {
            fs::rename(&k.c.novo, &k.c.atual).unwrap();
        }
        let mut s = Falso::novo(&k.c);
        s.estado = Servico::Parado; // reboot / serviço parado
        assert!(recuperar(&k.c, EXE, &mut s, &T0), "ponto {ponto}");
        assert_eq!(ler(&k.c.atual.join(EXE)), "BOM 1.0.6", "ponto {ponto}");
        assert_eq!(ler(&k.c.atual.join("custom.txt")), "config embutida");
        assert_eq!(s.estado, Servico::Rodando);
    }
}

#[test]
fn recuperacao_respeita_concluido() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    assert_eq!(
        k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s),
        Resultado::Atualizado
    );
    fs::create_dir_all(&k.c.troca).unwrap();
    fs::write(k.c.concluido(), "ok").unwrap();
    let mut s2 = Falso::novo(&k.c);
    assert!(recuperar(&k.c, EXE, &mut s2, &T0));
    assert_eq!(ler(&k.c.atual.join(EXE)), "BOM 1.0.7", "desfez uma troca concluída");
    assert!(!s2.eventos().iter().any(|e| e.starts_with("ren")));
}

#[test]
fn limpar_restos_nao_toca_a_atual() {
    let k = Cenario::novo("BOM 1.0.7");
    for d in [&k.c.novo, &k.c.anterior, &k.c.falhou, &k.c.troca] {
        escrever(&d.join("x"), "resto");
    }
    k.c.limpar_restos().unwrap();
    for d in [&k.c.novo, &k.c.anterior, &k.c.falhou, &k.c.troca] {
        assert!(!d.exists());
    }
    k.anterior_intacta_no_lugar();
}

#[test]
fn caminhos_irmaos() {
    let c = Caminhos::de(Path::new(r"C:\Program Files\SuporteVancin"));
    assert_eq!(c.novo, PathBuf::from(r"C:\Program Files\SuporteVancin.novo"));
    assert_eq!(c.anterior, PathBuf::from(r"C:\Program Files\SuporteVancin.anterior"));
    assert_eq!(c.troca.join("concluido"), c.concluido());
}

fn linha(s: &str, trecho: &str) -> usize {
    s.lines()
        .position(|l| l.contains(trecho))
        .unwrap_or_else(|| panic!("sem '{trecho}' no script:\n{s}"))
}

#[test]
fn script_guarda_espera_o_processo_antes_de_recuperar() {
    let c = Caminhos::de(Path::new("C:/Program Files/SuporteVancin"));
    let g = script_guarda(&c, 4242, 240).unwrap();
    assert!(g.contains("\r\n"));
    let concl = linha(&g, r#"if exist "%TROCA%\concluido" exit /b 0"#);
    let vivo = linha(&g, r#"tasklist /FI "PID eq 4242" /NH 2>nul | find " 4242 " >nul || goto recuperar"#);
    let chama = linha(&g, r#"call "%TROCA%\recuperar.bat""#);
    assert!(concl < vivo && vivo < chama);
    // nunca dá partida no serviço por conta própria
    assert!(!g.contains("sc start"));
    assert!(g.contains("if %N% GEQ 240 goto limite"));
}

#[test]
fn script_recuperar_segue_a_tabela() {
    let c = Caminhos::de(Path::new("C:/Program Files/SuporteVancin"));
    let r = script_recuperar(&c, "SuporteVancin", "RuntimeBroker_rustdesk.exe", "SuporteVancin-recuperar").unwrap();
    let concl = linha(&r, r#"if exist "%TROCA%\concluido" goto garantir"#);
    let sem_ant = linha(&r, r#"if not exist "%ANTERIOR%\SuporteVancin.exe" goto garantir"#);
    let parar = linha(&r, r#"sc stop "SuporteVancin""#);
    let tira = linha(&r, r#"if exist "%ATUAL%" call :ren "%ATUAL%" "SuporteVancin.falhou""#);
    let volta = linha(&r, r#"if not exist "%ATUAL%" call :ren "%ANTERIOR%" "SuporteVancin""#);
    let start = linha(&r, r#"sc start "SuporteVancin""#);
    assert!(concl < sem_ant && sem_ant < parar && parar < tira && tira < volta && volta < start);
    assert!(r.contains(r#"schtasks /Delete /F /TN "SuporteVancin-recuperar""#));
    // `move` aninharia a pasta se o destino existisse; só `ren`
    assert!(!r.lines().any(|l| l.trim_start().starts_with("move ")));
    assert!(r.contains(r#"set "ATUAL=C:/Program Files/SuporteVancin""#));
}

#[test]
fn scripts_recusam_caracteres_do_cmd() {
    for ruim in ["C:/A%B/X", "C:/A&B/X", "C:/A!B/X", "C:/A\"B/X", "C:/A^B/X"] {
        let c = Caminhos::de(Path::new(ruim));
        assert!(script_guarda(&c, 1, 1).is_err(), "{ruim}");
        assert!(script_recuperar(&c, "A", "B", "C").is_err(), "{ruim}");
    }
    let c = Caminhos::de(Path::new("C:/ok"));
    assert!(script_recuperar(&c, "A&B", "B", "C").is_err());
}

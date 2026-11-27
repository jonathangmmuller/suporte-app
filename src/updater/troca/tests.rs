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
        let c = Caminhos::de(
            &raiz.join("Program Files").join("SuporteVancin"),
            &raiz.join("ProgramData").join("SuporteVancin").join("atualizacao"),
        );
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
            criar_novo: criar_pasta,
        };
        let eventos = s.eventos.clone();
        let troca = self.c.troca.clone();
        atualizar(
            &self.c,
            &p,
            copiar,
            &mut |_| Ok(()),
            validar,
            &mut || {
                eventos.borrow_mut().push("guarda".to_owned());
                fs::create_dir_all(&troca)?;
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
    c: Caminhos,
    estado: Servico,
    consultas_ate_cair: Option<u32>,
    /// Nada sobe (serviço quebrado / conta do serviço sem permissão).
    quebrado: bool,
    /// pasta de origem → quantas vezes ainda falhar o rename
    falhas_renomear: HashMap<PathBuf, u32>,
    /// Tarefas da recuperação registradas.
    tarefas: bool,
    /// Operações até a "queda de energia" (pânico); `None` = sem queda.
    vida: Option<u32>,
    eventos: Rc<RefCell<Vec<String>>>,
}

impl Falso {
    fn novo(c: &Caminhos) -> Self {
        Self {
            c: c.clone(),
            estado: Servico::Rodando,
            consultas_ate_cair: None,
            quebrado: false,
            falhas_renomear: HashMap::new(),
            tarefas: true,
            vida: None,
            eventos: Rc::new(RefCell::new(Vec::new())),
        }
    }
    fn ev(&self, e: impl Into<String>) {
        self.eventos.borrow_mut().push(e.into());
    }
    fn eventos(&self) -> Vec<String> {
        self.eventos.borrow().clone()
    }
    fn operacao(&mut self) {
        if let Some(v) = self.vida.as_mut() {
            if *v == 0 {
                panic!("queda de energia simulada");
            }
            *v -= 1;
        }
    }
}

/// Invariante: sempre existe uma pasta instalada utilizável (com o EXE).
fn alguma_utilizavel(c: &Caminhos) -> bool {
    Pasta::TODAS.iter().any(|p| c.pasta(*p).join(EXE).is_file())
}

impl Sistema for Falso {
    fn servico(&mut self) -> Servico {
        self.operacao();
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
        self.operacao();
        self.ev("parar");
        self.estado = Servico::Parado;
        self.consultas_ate_cair = None;
    }
    fn iniciar_servico(&mut self) {
        self.operacao();
        let exe = ler(&self.c.atual.join(EXE));
        self.ev(format!("iniciar:{exe}"));
        if self.estado == Servico::Rodando || self.quebrado {
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
        self.operacao();
        self.ev("taskkill");
    }
    fn renomear(&mut self, de: &Path, para: &Path) -> io::Result<()> {
        self.operacao();
        if let Some(n) = self.falhas_renomear.get_mut(de) {
            if *n > 0 {
                *n -= 1;
                self.ev(format!("ren-falhou:{}", de.file_name().unwrap().to_string_lossy()));
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "em uso"));
            }
        }
        // como o `ren` do cmd e o `SistemaWindows`: destino existente = erro
        if para.exists() {
            self.ev("ren-destino-existe");
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "destino existe"));
        }
        self.ev(format!(
            "ren:{}>{}",
            de.file_name().unwrap().to_string_lossy(),
            para.file_name().unwrap().to_string_lossy()
        ));
        let r = fs::rename(de, para);
        assert!(alguma_utilizavel(&self.c), "sem instalação utilizável após {de:?}->{para:?}");
        r
    }
    fn esperar(&mut self, _d: Duration) {
        self.operacao();
    }
    fn apagar_tarefas(&mut self) {
        self.operacao();
        self.ev("apagar-tarefas");
        self.tarefas = false;
    }
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
        criar_novo: criar_pasta,
    };
    let r = atualizar(
        &k.c,
        &p,
        &mut copiar_arquivo,
        &mut |_| Ok(()),
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
            criar_novo: criar_pasta,
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
    escrever(&k.c.concluido(), "ok");
    k.c.limpar_restos().unwrap();
    for d in [&k.c.novo, &k.c.anterior, &k.c.falhou, &k.c.troca] {
        assert!(!d.exists());
    }
    k.anterior_intacta_no_lugar();
}

#[test]
fn caminhos_irmaos() {
    let c = Caminhos::de(
        Path::new(r"C:\Program Files\SuporteVancin"),
        Path::new(r"C:\ProgramData\SuporteVancin\atualizacao"),
    );
    assert_eq!(c.novo, PathBuf::from(r"C:\Program Files\SuporteVancin.novo"));
    assert_eq!(c.anterior, PathBuf::from(r"C:\Program Files\SuporteVancin.anterior"));
    // a pasta dos scripts SYSTEM não fica mais ao lado da instalação (achado 1)
    assert_eq!(c.troca, PathBuf::from(r"C:\ProgramData\SuporteVancin\atualizacao"));
    assert_eq!(c.troca.join("concluido"), c.concluido());
}


fn caminhos_windows() -> Caminhos {
    Caminhos::de(
        Path::new("C:/Program Files/SuporteVancin"),
        Path::new("C:/ProgramData/SuporteVancin/atualizacao"),
    )
}

// ------------------------------------------------------------------------------------
// R3, achado 1: conferência de ACL/reparse nos pontos de execução e de troca.
// ------------------------------------------------------------------------------------

impl Cenario {
    fn rodar_conferindo(
        &self,
        s: &mut Falso,
        conferir: &mut dyn FnMut(Momento) -> ResultType<()>,
        criar_novo: fn(&Path) -> io::Result<()>,
        executou: &mut bool,
    ) -> Resultado {
        let extras: Vec<(PathBuf, String)> = vec![];
        let p = Preparo {
            origem: &self.origem,
            exe_origem: "rustdesk.exe",
            exe_destino: EXE,
            extras: &extras,
            remover: &[],
            criar_novo,
        };
        let eventos = s.eventos.clone();
        let troca = self.c.troca.clone();
        atualizar(
            &self.c,
            &p,
            &mut copiar_arquivo,
            conferir,
            &mut |e| {
                *executou = true;
                validar_ok(e)
            },
            &mut || {
                eventos.borrow_mut().push("guarda".to_owned());
                fs::create_dir_all(&troca)?;
                Ok(())
            },
            s,
            true,
            &T0,
        )
    }
}

#[test]
fn acl_recusada_antes_de_executar_nao_executa_nem_para() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    let mut executou = false;
    let mut momentos = vec![];
    let r = k.rodar_conferindo(
        &mut s,
        &mut |m| {
            momentos.push(m);
            bail!("Usuários podem gravar na pasta-mãe")
        },
        criar_pasta,
        &mut executou,
    );
    assert!(matches!(r, Resultado::NaoTrocado(ref m) if m.contains("pasta-mãe")), "{r:?}");
    assert_eq!(momentos, vec![Momento::AntesDeExecutar]);
    assert!(!executou, "executou o EXE de uma pasta não protegida");
    assert!(s.eventos().is_empty());
    assert!(!k.c.novo.exists());
    k.anterior_intacta_no_lugar();
}

#[test]
fn acl_recusada_antes_de_trocar_nao_para_o_servico() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    let mut executou = false;
    let r = k.rodar_conferindo(
        &mut s,
        &mut |m| match m {
            Momento::AntesDeExecutar => Ok(()),
            Momento::AntesDeTrocar => bail!("recuperar.bat com hash diferente"),
        },
        criar_pasta,
        &mut executou,
    );
    assert!(matches!(r, Resultado::NaoTrocado(ref m) if m.contains("hash")), "{r:?}");
    assert!(executou);
    assert_eq!(s.eventos(), vec!["guarda".to_owned()], "parou o serviço");
    assert!(!k.c.novo.exists());
    k.anterior_intacta_no_lugar();
}

/// Alguém recria `.novo` entre a limpeza e a criação: a criação (que exige pasta nova, com a
/// DACL protegida) falha e nada é trocado.
#[test]
fn novo_recriado_por_terceiro_e_recusado() {
    fn criar_com_corrida(p: &Path) -> io::Result<()> {
        fs::create_dir_all(p)?; // "o atacante chegou antes"
        criar_pasta(p)
    }
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    let mut executou = false;
    let r = k.rodar_conferindo(&mut s, &mut |_| Ok(()), criar_com_corrida, &mut executou);
    assert!(matches!(r, Resultado::NaoTrocado(ref m) if m.contains(".novo")), "{r:?}");
    assert!(!executou && s.eventos().is_empty());
    k.anterior_intacta_no_lugar();
}

#[test]
fn recuperacao_pendente_impede_nova_atualizacao_e_limpeza() {
    let k = Cenario::novo("BOM 1.0.7");
    fs::rename(&k.c.atual, &k.c.anterior).unwrap(); // troca interrompida
    escrever(&k.c.troca.join(SCRIPT_RECUPERAR), "@echo off");
    assert!(k.c.recuperacao_pendente());
    let e = k.c.limpar_restos().unwrap_err().to_string();
    assert!(e.contains("pendente"), "{e}");
    assert_eq!(ler(&k.c.anterior.join(EXE)), "BOM 1.0.6", "apagou a única cópia");
    assert!(k.c.troca.join(SCRIPT_RECUPERAR).is_file());
    // depois de recuperada (concluido), a limpeza volta a valer
    let mut s = Falso::novo(&k.c);
    assert!(recuperar(&k.c, EXE, &mut s, &T0));
    assert!(!k.c.recuperacao_pendente());
    k.c.limpar_restos().unwrap();
    k.anterior_intacta_no_lugar();
}

// ------------------------------------------------------------------------------------
// R3, achado 3: partida não confirmada = Critico.
// ------------------------------------------------------------------------------------

#[test]
fn nada_trocado_mas_servico_nao_volta_e_critico() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    s.quebrado = true;
    s.falhas_renomear.insert(k.c.atual.clone(), 99);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::Critico(ref m) if m.contains("presa") && m.contains("RUNNING")), "{r:?}");
    assert!(!r.consistente());
    k.anterior_intacta_no_lugar();
}

#[test]
fn nova_nao_entra_e_servico_nao_volta_e_critico() {
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    s.quebrado = true;
    s.falhas_renomear.insert(k.c.novo.clone(), 99);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::Critico(ref m) if m.contains("não entrou")), "{r:?}");
    k.anterior_intacta_no_lugar();
}

/// A anterior volta, sobe e cai: R2 dizia `Revertido` (só esperava RUNNING uma vez). Agora é
/// `Critico`, e a recuperação fica pendente (tarefas mantidas, sem `concluido`).
#[test]
fn revertido_com_anterior_que_sobe_e_cai_e_critico_e_fica_pendente() {
    let k = Cenario::novo("RUIM 1.0.7");
    escrever(&k.c.atual.join(EXE), "CAI 1.0.6");
    let mut s = Falso::novo(&k.c);
    let r = k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
    assert!(matches!(r, Resultado::Critico(ref m) if m.contains("RUNNING")), "{r:?}");
    assert_eq!(ler(&k.c.atual.join(EXE)), "CAI 1.0.6");
    for _ in 0..3 {
        // tarefa de minuto em minuto
        let saida = executar(&programa_recuperar(), &k.c, EXE, &mut s, &T0, &mut |_, _| {});
        assert_eq!(saida, Saida::Pendente);
        assert!(s.tarefas, "apagou as tarefas sem RUNNING");
        assert!(!k.c.concluido().exists(), "gravou concluido sem RUNNING");
    }
}

// ------------------------------------------------------------------------------------
// R3, achado 2: a lógica do recuperar.bat, exercitada.
// ------------------------------------------------------------------------------------

const CONTEUDOS: [Option<&str>; 4] = [None, Some("BOM"), Some("RUIM"), Some("CAI")];

/// Só as três pastas + `troca` (mais leve que `Cenario`).
struct Mini {
    raiz: PathBuf,
    c: Caminhos,
}

impl Mini {
    fn novo(estado: [Option<&str>; 3], concluido: bool) -> Self {
        let raiz = tmp();
        let c = Caminhos::de(
            &raiz.join("PF").join("SuporteVancin"),
            &raiz.join("PD").join("atualizacao"),
        );
        fs::create_dir_all(&c.troca).unwrap();
        for (p, v) in Pasta::TODAS.iter().zip(estado) {
            if let Some(v) = v {
                escrever(&c.pasta(*p).join(EXE), &format!("{v} {p:?}"));
            }
        }
        if concluido {
            fs::write(c.concluido(), "ok").unwrap();
        }
        Self { raiz, c }
    }
}

impl Drop for Mini {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.raiz);
    }
}

fn recuperar_conferindo(c: &Caminhos, s: &mut Falso) -> Saida {
    executar(&programa_recuperar(), c, EXE, s, &T0, &mut |i, p| {
        assert!(alguma_utilizavel(c), "passo {i} ({p:?}) deixou sem instalação utilizável");
    })
}

/// Todos os estados iniciais (cada pasta ausente/boa/ruim/sobe-e-cai, com e sem `concluido`),
/// todos os padrões de falha de `ren` (0, 1 = passageira, 99 = permanente) por pasta, serviço
/// parado ou rodando: a invariante vale em todo passo; `Concluida` só com a atual utilizável,
/// RUNNING, `concluido` gravado e tarefas apagadas; `Pendente` mantém as tarefas e não grava
/// `concluido`; a anterior boa nunca é destruída; com `concluido`, nada é renomeado.
#[test]
fn recuperacao_exaustiva() {
    let mut casos = 0;
    let mut concluidas = 0;
    for a in CONTEUDOS {
        for b in CONTEUDOS {
            for f in CONTEUDOS {
                let estado = [a, b, f];
                if estado.iter().all(Option::is_none) {
                    continue; // fora da invariante desde o início
                }
                for concluido in [false, true] {
                    for padrao in 0..27usize {
                        let falhas = [padrao % 3, padrao / 3 % 3, padrao / 9].map(|i| [0, 1, 99][i]);
                        for inicial in [Servico::Rodando, Servico::Parado] {
                            casos += 1;
                            let m = Mini::novo(estado, concluido);
                            let mut s = Falso::novo(&m.c);
                            s.estado = inicial;
                            for (p, n) in Pasta::TODAS.iter().zip(falhas) {
                                if n > 0 {
                                    s.falhas_renomear.insert(m.c.pasta(*p).to_owned(), n);
                                }
                            }
                            let anterior = ler(&m.c.anterior.join(EXE));
                            let saida = recuperar_conferindo(&m.c, &mut s);
                            let ctx = format!(
                                "{estado:?} concluido={concluido} falhas={falhas:?} {inicial:?} -> {saida:?} {:?}",
                                s.eventos()
                            );
                            match saida {
                                Saida::Concluida => {
                                    concluidas += 1;
                                    assert!(m.c.atual.join(EXE).is_file(), "{ctx}");
                                    assert_eq!(s.estado, Servico::Rodando, "{ctx}");
                                    assert!(m.c.concluido().is_file(), "{ctx}");
                                    assert!(!s.tarefas, "{ctx}");
                                }
                                Saida::Pendente => {
                                    assert!(s.tarefas, "apagou tarefas: {ctx}");
                                    assert_eq!(m.c.concluido().exists(), concluido, "{ctx}");
                                }
                            }
                            if anterior.starts_with("BOM") && !concluido {
                                assert!(
                                    Pasta::TODAS
                                        .iter()
                                        .any(|p| ler(&m.c.pasta(*p).join(EXE)) == anterior),
                                    "a anterior boa sumiu: {ctx}"
                                );
                            }
                            if concluido && a.is_some() {
                                assert!(!s.eventos().iter().any(|e| e.starts_with("ren")), "{ctx}");
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(casos, 63 * 2 * 27 * 2);
    assert!(concluidas > 0 && concluidas < casos);
}

/// Estados que uma troca interrompida deixa (a anterior boa em `.anterior`), com o `ren`
/// bloqueado por um tempo: cada disparo da tarefa fica `Pendente` sem estragar nada e, quando
/// o bloqueio some, converge para a anterior no lugar e o serviço rodando.
#[test]
fn recuperacao_repetida_converge_quando_o_bloqueio_some() {
    let estados = [
        [None, Some("BOM"), None],
        [Some("RUIM"), Some("BOM"), None],
        [Some("CAI"), Some("BOM"), None],
        [None, Some("BOM"), Some("RUIM")],
        [Some("RUIM"), Some("BOM"), Some("CAI")],
    ];
    for estado in estados {
        for bloqueada in [Pasta::Atual, Pasta::Anterior, Pasta::Falhou] {
            let m = Mini::novo(estado, false);
            let mut s = Falso::novo(&m.c);
            s.estado = Servico::Parado;
            s.falhas_renomear.insert(m.c.pasta(bloqueada).to_owned(), 99);
            let mut saidas = vec![];
            for disparo in 0..6 {
                if disparo == 3 {
                    s.falhas_renomear.clear(); // antivírus soltou a pasta
                }
                let saida = recuperar_conferindo(&m.c, &mut s);
                saidas.push(saida);
                if saida == Saida::Concluida {
                    break;
                }
            }
            let ctx = format!("{estado:?} bloqueada={bloqueada:?} {saidas:?} {:?}", s.eventos());
            assert_eq!(saidas.last(), Some(&Saida::Concluida), "{ctx}");
            assert_eq!(ler(&m.c.atual.join(EXE)), "BOM Anterior", "{ctx}");
            assert_eq!(s.estado, Servico::Rodando, "{ctx}");
            assert!(!s.tarefas, "{ctx}");
        }
    }
}

fn queda(r: std::thread::Result<impl Sized>) -> bool {
    match r {
        Ok(_) => false,
        Err(e) => {
            let msg = e
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| e.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            assert_eq!(msg, "queda de energia simulada", "pânico que não é a queda simulada");
            true
        }
    }
}

/// Queda de energia em cada operação da troca e, depois do boot, em cada passo da
/// recuperação; uma última recuperação completa sempre termina com a 1.0.6 (ou a 1.0.7 boa)
/// no lugar, o serviço RUNNING e as tarefas apagadas. A invariante é conferida em todo `ren`
/// e em todo passo.
#[test]
fn queda_de_energia_em_cada_ponto_da_troca_e_da_recuperacao() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let passos = programa_recuperar().len() as u32;
    for novo in ["RUIM 1.0.7", "CAI 1.0.7", "BOM 1.0.7"] {
        let total = {
            let k = Cenario::novo(novo);
            let mut s = Falso::novo(&k.c);
            s.vida = Some(u32::MAX);
            k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s);
            u32::MAX - s.vida.unwrap()
        };
        assert!(total > 10, "{novo}: {total}");
        for k1 in 0..total {
            // k2 = passos: recuperação sem queda
            for k2 in (0..=passos).step_by(if k1 % 4 == 0 { 1 } else { 7 }) {
                let k = Cenario::novo(novo);
                let mut s = Falso::novo(&k.c);
                s.vida = Some(k1);
                let caiu = queda(catch_unwind(AssertUnwindSafe(|| {
                    k.rodar(&mut copiar_arquivo, &mut validar_ok, &mut s)
                })));
                assert!(caiu, "{novo} k1={k1}");
                assert!(alguma_utilizavel(&k.c), "{novo} k1={k1}");
                // boot: serviço parado; tarefa ao iniciar, que também pode cair
                let mut s2 = Falso::novo(&k.c);
                s2.estado = Servico::Parado;
                let mut n = 0u32;
                let r = catch_unwind(AssertUnwindSafe(|| {
                    executar(&programa_recuperar(), &k.c, EXE, &mut s2, &T0, &mut |i, p| {
                        assert!(alguma_utilizavel(&k.c), "passo {i} {p:?}");
                        n += 1;
                        if n == k2 {
                            panic!("queda de energia simulada");
                        }
                    })
                }));
                let primeira = if queda(r) { None } else { Some(()) };
                let mut s3 = Falso::novo(&k.c);
                s3.estado = Servico::Parado;
                let saida = recuperar_conferindo(&k.c, &mut s3);
                let ctx = format!("{novo} k1={k1} k2={k2} 1a={primeira:?} {:?}", s3.eventos());
                assert_eq!(saida, Saida::Concluida, "{ctx}");
                assert_eq!(s3.estado, Servico::Rodando, "{ctx}");
                assert!(!s3.tarefas, "{ctx}");
                let exe = ler(&k.c.atual.join(EXE));
                if novo.starts_with("BOM") {
                    assert!(exe.starts_with("BOM"), "{ctx}: {exe}");
                } else {
                    assert_eq!(exe, "BOM 1.0.6", "{ctx}");
                    assert_eq!(ler(&k.c.atual.join("custom.txt")), "config embutida", "{ctx}");
                }
            }
        }
    }
}

#[test]
fn programa_invalido_e_recusado() {
    use Passo::*;
    assert!(validar_programa(&programa_recuperar()).is_ok());
    assert!(validar_programa(&[Ir("x"), Sair(Saida::Pendente)]).is_err());
    assert!(validar_programa(&[Rotulo("a"), Rotulo("a"), Sair(Saida::Pendente)]).is_err());
    assert!(validar_programa(&[Rotulo("a&b"), Sair(Saida::Pendente)]).is_err());
    assert!(validar_programa(&[Log("x>y"), Sair(Saida::Pendente)]).is_err());
    assert!(validar_programa(&[Log("fim")]).is_err(), "cai do fim");
}

// ------------------------------------------------------------------------------------
// Gerador do recuperar.bat: cada passo vira o trecho equivalente.
// ------------------------------------------------------------------------------------

const TAREFAS: [&str; 2] = [
    "SuporteVancin-recuperar-atualizacao",
    "SuporteVancin-recuperar-atualizacao-minuto",
];

fn bat() -> String {
    script_recuperar(
        &caminhos_windows(),
        "SuporteVancin",
        "RuntimeBroker_rustdesk.exe",
        &TAREFAS,
        &Tempos::PADRAO,
    )
    .unwrap()
}

fn linhas(s: &str) -> Vec<&str> {
    s.split("\r\n").collect()
}

fn linha(s: &str, trecho: &str) -> usize {
    s.lines()
        .position(|l| l.contains(trecho))
        .unwrap_or_else(|| panic!("sem '{trecho}' no script:\n{s}"))
}

#[test]
fn bat_todo_goto_tem_rotulo_e_todo_rotulo_do_programa_existe() {
    let b = bat();
    let ls = linhas(&b);
    let rotulos: Vec<&str> = ls.iter().filter_map(|l| l.strip_prefix(':')).collect();
    let mut unicos = rotulos.clone();
    unicos.sort();
    unicos.dedup();
    assert_eq!(unicos.len(), rotulos.len(), "rótulo repetido");
    for l in &ls {
        if let Some(i) = l.find("goto ") {
            let alvo = l[i + 5..].split_whitespace().next().unwrap();
            assert!(rotulos.contains(&alvo), "goto sem rótulo: {l}");
        }
        if let Some(i) = l.find("call :") {
            let alvo = l[i + 6..].split_whitespace().next().unwrap();
            assert!(rotulos.contains(&alvo), "call sem rótulo: {l}");
        }
    }
    for p in programa_recuperar() {
        if let Passo::Rotulo(r) = p {
            assert!(rotulos.contains(&r), "{r}");
        }
    }
    assert!(!ls.iter().any(|l| l.trim_start().starts_with("move ")));
    assert!(b.contains(r#"set "ATUAL=C:/Program Files/SuporteVancin""#));
    assert!(b.contains(r#"set "TROCA=C:/ProgramData/SuporteVancin/atualizacao""#));
}

/// Recuperação serializada: o corpo só roda com o descritor 9 em `recuperar.lock`; sem a
/// trava, sai com 3 sem tocar em nada. `:principal` não é alcançável por queda.
#[test]
fn bat_corpo_so_roda_com_a_trava() {
    let b = bat();
    let ls = linhas(&b);
    let chama = linha(&b, r#"call :principal 9>"%TROCA%\recuperar.lock""#);
    let sai = linha(&b, "exit /b %SAIDA%");
    let corpo = ls.iter().position(|l| *l == ":principal").unwrap();
    assert!(linha(&b, r#"set "SAIDA=3""#) < chama && chama < sai && sai + 1 == corpo);
    // nada antes da trava mexe em serviço, pasta ou tarefa
    for l in &ls[..corpo] {
        for proibido in ["sc ", "ren ", "rd ", "schtasks", "taskkill"] {
            assert!(!l.contains(proibido), "{l}");
        }
    }
}

/// `schtasks /Delete` e `concluido` só são alcançáveis passando pela confirmação de RUNNING
/// estável: o rótulo imediatamente anterior é o fim do laço de estabilidade, que só é
/// alcançado depois de `find "RUNNING"` ter dado certo, e não há queda para dentro dele.
#[test]
fn bat_so_apaga_tarefas_e_conclui_depois_de_running_estavel() {
    let b = bat();
    let ls = linhas(&b);
    let alvos: Vec<usize> = ls
        .iter()
        .enumerate()
        .filter(|(_, l)| l.contains("schtasks /Delete") || l.contains("echo ok>"))
        .map(|(i, _)| i)
        .collect();
    assert_eq!(alvos.len(), 3, "2 tarefas + concluido");
    for i in alvos {
        let r = (0..i).rev().find(|j| ls[*j].starts_with(':')).unwrap();
        let rot = &ls[r][1..];
        let k = rot.strip_suffix("_fim").unwrap_or_else(|| panic!("{} vem de {rot}", ls[i]));
        // entre o rótulo e a linha só há marcar concluido / apagar tarefas (sem saltos)
        for l in &ls[r + 1..i] {
            assert!(
                l.contains("echo ok>") || l.contains("if not exist \"%TROCA%\\concluido\" goto pendente") || l.contains("schtasks /Delete"),
                "{l}"
            );
        }
        assert_eq!(ls[r - 1], format!("goto {k}_e"), "queda para dentro de {rot}");
        let gotos: Vec<&&str> = ls.iter().filter(|l| l.ends_with(&format!("goto {rot}"))).collect();
        assert_eq!(gotos, vec![&format!("if %N% GEQ 20 goto {rot}").as_str()]);
        let e = ls.iter().position(|l| *l == format!(":{k}_e")).unwrap();
        let ok = ls.iter().position(|l| *l == format!(":{k}_ok")).unwrap();
        assert_eq!(ls[e + 1], format!("if %N% GEQ 20 goto {rot}"));
        assert_eq!(ls[e + 3], r#"sc query "SuporteVancin" | find "RUNNING" >nul || goto pendente"#);
        assert_eq!(ls[ok - 1], format!("goto {k}"), "queda para dentro de {k}_ok");
        let entra_ok: Vec<&&str> = ls.iter().filter(|l| l.ends_with(&format!("goto {k}_ok"))).collect();
        assert_eq!(
            entra_ok,
            vec![&format!(r#"sc query "SuporteVancin" | find "RUNNING" >nul && goto {k}_ok"#).as_str()]
        );
    }
    // a única partida do serviço é a do passo SubirEstavel
    assert_eq!(ls.iter().filter(|l| l.contains("sc start")).count(), 1);
}

#[test]
fn bat_cada_ren_confere_resultado_e_destino() {
    let b = bat();
    let ls = linhas(&b);
    let ren: Vec<usize> = (0..ls.len()).filter(|i| ls[*i].starts_with("call :ren ")).collect();
    let n = programa_recuperar()
        .iter()
        .filter(|p| matches!(p, Passo::Renomear { .. }))
        .count();
    assert_eq!(ren.len(), n);
    for i in ren {
        assert!(ls[i + 1].starts_with("if errorlevel 1 goto "), "{}", ls[i + 1]);
        assert!(ls[i + 2].starts_with("if not exist \"%"), "{}", ls[i + 2]);
    }
    let sub = ls.iter().position(|l| *l == ":ren").unwrap();
    assert_eq!(ls[sub + 1], r#"if exist "%~3" exit /b 1"#);
    assert!(b.contains("if %R% GEQ 20 exit /b 1"));
    // trecho do passo "anterior volta; se não voltar, a que falhou volta" (achado 2)
    let restaurar = linha(&b, r#"call :ren "%ANTERIOR%" "SuporteVancin" "%ATUAL%""#);
    assert_eq!(ls[restaurar + 1], "if errorlevel 1 goto devolver");
    let devolver = ls.iter().position(|l| *l == ":devolver").unwrap();
    assert!(ls[devolver..]
        .iter()
        .take(8)
        .any(|l| *l == r#"call :ren "%FALHOU%" "SuporteVancin" "%ATUAL%""#));
}

#[test]
fn bat_segue_os_tempos() {
    let t = Tempos {
        passo: Duration::from_secs(1),
        parar: 7,
        subir: 8,
        estavel: 9,
        renomear: 4,
    };
    let b = script_recuperar(&caminhos_windows(), "A", "B", &["T"], &t).unwrap();
    for trecho in [
        "if %N% GEQ 7 goto",
        "if %N% GEQ 8 goto pendente",
        "if %N% GEQ 9 goto",
        "if %R% GEQ 4 exit /b 1",
    ] {
        assert!(b.contains(trecho), "{trecho}");
    }
}

#[test]
fn script_guarda_espera_o_processo_antes_de_recuperar() {
    let c = caminhos_windows();
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
fn scripts_recusam_caracteres_do_cmd() {
    for ruim in ["C:/A%B/X", "C:/A&B/X", "C:/A!B/X", "C:/A\"B/X", "C:/A^B/X"] {
        let c = Caminhos::de(Path::new(ruim), Path::new("C:/PD/x"));
        assert!(script_recuperar(&c, "A", "B", &["C"], &T0).is_err(), "{ruim}");
        let c = Caminhos::de(Path::new("C:/ok"), Path::new(ruim));
        assert!(script_guarda(&c, 1, 1).is_err(), "{ruim}");
        assert!(script_recuperar(&c, "A", "B", &["C"], &T0).is_err(), "{ruim}");
    }
    let c = caminhos_windows();
    assert!(script_recuperar(&c, "A&B", "B", &["C"], &T0).is_err());
    assert!(script_recuperar(&c, "A", "B", &["C|D"], &T0).is_err());
}

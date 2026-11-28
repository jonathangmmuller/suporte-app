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
    // espera do manual (em passos) maior que a execução mais longa do corpo, como os 20 min de
    // `Tempos::PADRAO` diante do pior caso de ≈ 8 min
    trava: 200,
};

fn tmp() -> PathBuf {
    static N: AtomicU32 = AtomicU32::new(0);
    // canônico: no macOS `/var` é link simbólico (os testes de reparse usam links reais)
    let d = fs::canonicalize(std::env::temp_dir()).unwrap().join(format!(
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
        fs::create_dir_all(raiz.join("ProgramData")).unwrap(); // existe em todo Windows
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
    /// Conteúdo do EXE que o serviço está rodando (vale com `estado == Rodando`).
    rodando: Option<String>,
    consultas_ate_cair: Option<u32>,
    /// Nada sobe (serviço quebrado / conta do serviço sem permissão).
    quebrado: bool,
    /// pasta de origem → quantas vezes ainda falhar o rename
    falhas_renomear: HashMap<PathBuf, u32>,
    /// Tarefas fixas da recuperação registradas (ao iniciar e a cada minuto).
    tarefas: bool,
    /// As fixas habilitadas (o modo manual desabilita).
    habilitadas: bool,
    /// Tarefa `religar` registrada.
    religar: bool,
    /// Processos segurando `recuperar.lock` agora (e o máximo visto).
    donos: u32,
    max_donos: u32,
    /// Mutação: a trava não exclui (prova que os testes de disputa enxergam a falta dela).
    trava_quebrada: bool,
    /// `rd /s /q` de `.falhou` falha (arquivo preso) e a pasta fica.
    falhou_presa: bool,
    /// Operações de agenda que falham: "criar_religar", "desabilitar", "habilitar".
    falhar_agenda: Vec<&'static str>,
    /// Operações até a "queda de energia" (pânico); `None` = sem queda.
    vida: Option<u32>,
    eventos: Rc<RefCell<Vec<String>>>,
}

impl Falso {
    /// Estado coerente com o disco (R4, item 5): só começa RUNNING se o EXE em `atual` sobe
    /// (`BOM`, ou `CAI`, que cai logo); senão começa parado.
    fn novo(c: &Caminhos) -> Self {
        let exe = ler(&c.atual.join(EXE));
        let sobe = exe.starts_with("BOM") || exe.starts_with("CAI");
        Self {
            c: c.clone(),
            estado: if sobe { Servico::Rodando } else { Servico::Parado },
            rodando: sobe.then(|| exe.clone()),
            consultas_ate_cair: exe.starts_with("CAI").then_some(2),
            quebrado: false,
            falhas_renomear: HashMap::new(),
            tarefas: true,
            habilitadas: true,
            religar: false,
            donos: 0,
            max_donos: 0,
            trava_quebrada: false,
            falhou_presa: false,
            falhar_agenda: vec![],
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

/// Invariante durante a recuperação: sempre existe uma pasta com o EXE (nenhum passo destrói
/// a última cópia). Não basta para "concluída": ver `ativo_utilizavel`.
fn alguma_utilizavel(c: &Caminhos) -> bool {
    Pasta::TODAS.iter().any(|p| c.pasta(*p).join(EXE).is_file())
}

/// Invariante literal de toda recuperação `Concluida` (R4, item 5): o caminho ativo
/// (`InstallLocation` = `atual`) tem uma instalação que sobe e fica (`BOM`), e o serviço está
/// RUNNING com ESSE conteúdo. EXE ruim/que cai, ou boa em outra pasta, não conta.
fn ativo_utilizavel(c: &Caminhos, s: &Falso) -> Result<(), String> {
    let exe = ler(&c.atual.join(EXE));
    if !exe.starts_with("BOM") {
        return Err(format!("caminho ativo com {exe:?}"));
    }
    if s.estado != Servico::Rodando || s.rodando.as_deref() != Some(exe.as_str()) {
        return Err(format!("serviço {:?} rodando {:?}, ativo {exe:?}", s.estado, s.rodando));
    }
    Ok(())
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
        // Já rodando: continua rodando o que rodava (ERROR_SERVICE_ALREADY_RUNNING). Como a
        // pasta em uso não pode ser renomeada (`renomear`), é sempre o conteúdo de `atual`.
        if self.estado == Servico::Rodando || self.quebrado {
            return;
        }
        if exe.starts_with("BOM") {
            self.estado = Servico::Rodando;
            self.rodando = Some(exe);
        } else if exe.starts_with("CAI") {
            self.estado = Servico::Rodando;
            self.rodando = Some(exe);
            self.consultas_ate_cair = Some(2);
        }
    }
    fn encerrar_processos(&mut self) {
        self.operacao();
        self.ev("taskkill");
    }
    fn renomear(&mut self, de: &Path, para: &Path) -> io::Result<()> {
        self.operacao();
        // Como no Windows: a pasta do EXE em execução não se renomeia.
        if self.estado == Servico::Rodando && de == self.c.atual {
            self.ev("ren-em-uso");
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "EXE em execução"));
        }
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
        self.religar = false;
    }
    fn apagar(&mut self, p: &Path) -> io::Result<()> {
        self.operacao();
        if self.falhou_presa && p == self.c.falhou && p.exists() {
            self.ev("rd-preso");
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, "arquivo preso"));
        }
        remover_arvore(p).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
    fn travar(&mut self) -> bool {
        self.operacao();
        if self.donos > 0 && !self.trava_quebrada {
            self.ev("trava-ocupada");
            return false;
        }
        self.donos += 1;
        self.max_donos = self.max_donos.max(self.donos);
        self.ev("travou");
        true
    }
    fn soltar(&mut self) {
        // fechar o handle (ou o processo morrer) não é uma operação que possa "cair"
        self.donos -= 1;
        self.ev("soltou");
    }
    fn criar_religar(&mut self) -> bool {
        self.operacao();
        if self.falhar_agenda.contains(&"criar_religar") {
            return false;
        }
        self.ev("criar-religar");
        self.religar = true;
        true
    }
    fn desabilitar_tarefas(&mut self) -> bool {
        self.operacao();
        if self.falhar_agenda.contains(&"desabilitar") && self.tarefas {
            return false;
        }
        self.ev("desabilitar");
        self.habilitadas = false;
        true
    }
    fn habilitar_tarefas(&mut self) -> bool {
        self.operacao();
        if self.falhar_agenda.contains(&"habilitar") && self.tarefas {
            return false;
        }
        self.ev("habilitar");
        self.habilitadas = true;
        true
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

// ------------------------------------------------------------------------------------
// R4, achados 1 e 2: cadeia de `troca` conferida antes de ler marcadores ou apagar restos;
// preparação incompleta x troca iniciada.
// ------------------------------------------------------------------------------------

const FULANO: &str = "S-1-5-21-1-2-3-1001";

fn protegido() -> acl::Descritor {
    acl::Descritor {
        dono: acl::SID_SYSTEM.to_owned(),
        dacl: Some(vec![]),
        reparse: false,
    }
}

/// Segurança simulada sobre pastas reais: link simbólico real = reparse point; o resto é
/// protegido, salvo os `trocados`.
fn ler_seguranca(
    trocados: Vec<(PathBuf, acl::Descritor)>,
) -> impl FnMut(&Path) -> io::Result<acl::Descritor> {
    move |p: &Path| {
        if let Some((_, d)) = trocados.iter().find(|(q, _)| q == p) {
            return Ok(d.clone());
        }
        // como o `GetNamedSecurityInfoW`: caminho inexistente = erro (recusa)
        let link = fs::symlink_metadata(p)?.file_type().is_symlink();
        Ok(acl::Descritor {
            reparse: link,
            ..protegido()
        })
    }
}

fn travar_arquivo(p: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().write(true).create(true).open(p)
}

/// `abrir_troca` com criação/tarefas registradas: (resultado, pastas criadas, apagou tarefas).
fn abrir_com(
    c: &Caminhos,
    ler: &mut dyn FnMut(&Path) -> io::Result<acl::Descritor>,
    travar: &mut dyn FnMut(&Path) -> io::Result<fs::File>,
) -> (ResultType<fs::File>, Vec<PathBuf>, bool) {
    let mut criadas = vec![];
    let mut apagou = false;
    let r = abrir_troca(
        c,
        EXE,
        &mut Abrir {
            ler,
            criar: &mut |p: &Path| {
                criadas.push(p.to_owned());
                criar_pasta(p)
            },
            travar,
            apagar_tarefas: &mut || apagou = true,
        },
    );
    (r, criadas, apagou)
}

/// Restos de uma atualização anterior (irmãs da instalação e arquivos de `troca`).
fn plantar_restos(c: &Caminhos, marcadores: &[&str]) -> Vec<PathBuf> {
    let mut v = vec![];
    for d in [&c.novo, &c.anterior, &c.falhou, &c.afastada(1), &c.afastada(AFASTADAS)] {
        escrever(&d.join(EXE), "BOM resto");
        v.push(d.join(EXE));
    }
    for n in [SCRIPT_RECUPERAR, SCRIPT_GUARDA].iter().chain(marcadores) {
        escrever(&c.troca.join(n), "resto");
        v.push(c.troca.join(n));
    }
    v
}

fn nada_apagado(restos: &[PathBuf], ctx: &str) {
    for r in restos {
        assert!(r.is_file(), "{ctx}: apagou {r:?}");
    }
}

#[test]
fn abrir_troca_descarta_restos_de_troca_concluida_sem_tocar_a_atual() {
    let k = Cenario::novo("BOM 1.0.7");
    let restos = plantar_restos(&k.c, &[MARCADOR_INICIADA, MARCADOR_CONCLUIDO]);
    let (r, criadas, apagou) = abrir_com(&k.c, &mut ler_seguranca(vec![]), &mut travar_arquivo);
    r.unwrap();
    assert!(criadas.is_empty() && apagou);
    for x in &restos {
        assert!(!x.exists(), "sobrou {x:?}");
    }
    assert!(k.c.troca.join(TRAVA).is_file());
    assert_eq!(k.c.fase(), Fase::PreparacaoIncompleta);
    k.anterior_intacta_no_lugar();
}

#[test]
fn abrir_troca_cria_base_e_troca_ja_conferidas() {
    let k = Cenario::novo("BOM 1.0.7");
    let base = k.c.troca.parent().unwrap().to_owned();
    assert!(!base.exists());
    let (r, criadas, _) = abrir_com(&k.c, &mut ler_seguranca(vec![]), &mut travar_arquivo);
    r.unwrap();
    assert_eq!(criadas, vec![base, k.c.troca.clone()]);
}

/// `%ProgramData%\<App>` pré-criada por um usuário comum (dono dele), com ou sem
/// `atualizacao` dentro: recusa sem ler marcador, sem apagar nada, sem criar nada e sem mexer
/// nas tarefas.
#[test]
fn base_criada_por_usuario_comum_nao_causa_exclusao() {
    for com_troca in [true, false] {
        let k = Cenario::novo("BOM 1.0.7");
        let base = k.c.troca.parent().unwrap().to_owned();
        let restos = if com_troca {
            plantar_restos(&k.c, &[MARCADOR_CONCLUIDO])
        } else {
            let mut v = vec![];
            for d in [&k.c.novo, &k.c.anterior, &k.c.falhou] {
                escrever(&d.join(EXE), "BOM resto");
                v.push(d.join(EXE));
            }
            escrever(&base.join("plantado.txt"), "x");
            v.push(base.join("plantado.txt"));
            v
        };
        let mut do_usuario = protegido();
        do_usuario.dono = FULANO.to_owned();
        let (r, criadas, apagou) = abrir_com(
            &k.c,
            &mut ler_seguranca(vec![(base.clone(), do_usuario)]),
            &mut travar_arquivo,
        );
        let e = r.unwrap_err().to_string();
        let ctx = format!("com_troca={com_troca}: {e}");
        assert!(e.contains("dono") && e.contains(FULANO), "{ctx}");
        assert!(criadas.is_empty() && !apagou, "{ctx}");
        nada_apagado(&restos, &ctx);
        assert!(!k.c.troca.join(TRAVA).exists(), "{ctx}");
        k.anterior_intacta_no_lugar();
    }
}

/// Junção (reparse point) num ancestral de `troca` ou da instalação: recusa sem apagar nada.
#[test]
fn juncao_em_ancestral_nao_causa_exclusao() {
    for qual in ["ProgramData", "Program Files", "raiz"] {
        let k = Cenario::novo("BOM 1.0.7");
        let restos = plantar_restos(&k.c, &[MARCADOR_CONCLUIDO]);
        let alvo = match qual {
            "raiz" => k.raiz.clone(),
            n => k.raiz.join(n),
        };
        let mut j = protegido();
        j.reparse = true;
        let (r, criadas, apagou) =
            abrir_com(&k.c, &mut ler_seguranca(vec![(alvo, j)]), &mut travar_arquivo);
        let e = r.unwrap_err().to_string();
        assert!(e.contains("reparse"), "{qual}: {e}");
        assert!(criadas.is_empty() && !apagou, "{qual}");
        nada_apagado(&restos, qual);
        k.anterior_intacta_no_lugar();
    }
}

/// O mesmo com um link simbólico REAL no ancestral: `ProgramData` aponta para outra pasta
/// cheia de "restos". Nada do outro lado é lido como marcador nem apagado.
#[cfg(unix)]
#[test]
fn link_real_em_ancestral_nao_causa_exclusao() {
    let k = Cenario::novo("BOM 1.0.7");
    let outra = k.raiz.join("outra");
    let plantados: Vec<PathBuf> = ["SuporteVancin/atualizacao/concluido", "SuporteVancin/atualizacao/recuperar.bat", "SuporteVancin/importante.txt"]
        .iter()
        .map(|r| {
            let p = outra.join(r);
            escrever(&p, "do outro lado");
            p
        })
        .collect();
    fs::remove_dir(k.raiz.join("ProgramData")).unwrap();
    std::os::unix::fs::symlink(&outra, k.raiz.join("ProgramData")).unwrap();
    assert!(k.c.concluido().is_file(), "o link resolve");
    let (r, criadas, apagou) = abrir_com(&k.c, &mut ler_seguranca(vec![]), &mut travar_arquivo);
    let e = r.unwrap_err().to_string();
    assert!(e.contains("reparse") && e.contains("ProgramData"), "{e}");
    assert!(criadas.is_empty() && !apagou);
    nada_apagado(&plantados, "link real");
    assert!(!outra.join("SuporteVancin/atualizacao").join(TRAVA).exists());
}

#[test]
fn trava_ocupada_nao_apaga_nada() {
    let k = Cenario::novo("BOM 1.0.7");
    let restos = plantar_restos(&k.c, &[MARCADOR_CONCLUIDO]);
    let (r, _, apagou) = abrir_com(&k.c, &mut ler_seguranca(vec![]), &mut |_| {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "em uso por outro processo"))
    });
    assert!(r.unwrap_err().to_string().contains("trava"));
    assert!(!apagou);
    nada_apagado(&restos, "trava ocupada");
}

/// `iniciada` sem `concluido`: a troca é da recuperação. Recusa sem apagar e sem mexer nas
/// tarefas; depois de a recuperação concluir, a próxima atualização descarta e segue.
#[test]
fn troca_iniciada_recusa_sem_tocar_e_libera_depois_da_recuperacao() {
    let k = Cenario::novo("BOM 1.0.7");
    fs::rename(&k.c.atual, &k.c.anterior).unwrap(); // troca interrompida
    escrever(&k.c.troca.join(SCRIPT_RECUPERAR), "@echo off");
    escrever(&k.c.iniciada(), "ok");
    assert_eq!(k.c.fase(), Fase::TrocaIniciada);
    assert!(k.c.recuperacao_pendente());
    // sem a instalação no caminho ativo: recusa já na cadeia, sem ler marcador
    let (r, _, apagou) = abrir_com(&k.c, &mut ler_seguranca(vec![]), &mut travar_arquivo);
    assert!(r.unwrap_err().to_string().contains("SuporteVancin"));
    assert!(!apagou && k.c.anterior.join(EXE).is_file());
    // nova (ruim) já no lugar: a fase manda
    escrever(&k.c.atual.join(EXE), "RUIM 1.0.7");
    let (r, _, apagou) = abrir_com(&k.c, &mut ler_seguranca(vec![]), &mut travar_arquivo);
    let e = r.unwrap_err().to_string();
    assert!(e.contains("iniciada e não concluída"), "{e}");
    assert!(!apagou, "apagou as tarefas da recuperação");
    assert_eq!(ler(&k.c.anterior.join(EXE)), "BOM 1.0.6", "apagou a única cópia");
    assert!(k.c.troca.join(SCRIPT_RECUPERAR).is_file());
    let mut s = Falso::novo(&k.c);
    assert!(recuperar(&k.c, EXE, &mut s, &T0));
    assert_eq!(k.c.fase(), Fase::Concluida);
    let (r, _, apagou) = abrir_com(&k.c, &mut ler_seguranca(vec![]), &mut travar_arquivo);
    r.unwrap();
    assert!(apagou && !k.c.iniciada().exists() && !k.c.concluido().exists());
    k.anterior_intacta_no_lugar();
}

#[test]
fn atual_sem_exe_nao_apaga_anterior_nem_falhou() {
    let k = Cenario::novo("BOM 1.0.7");
    fs::remove_file(k.c.atual.join(EXE)).unwrap();
    let restos = plantar_restos(&k.c, &[]);
    let (r, _, _) = abrir_com(&k.c, &mut ler_seguranca(vec![]), &mut travar_arquivo);
    assert!(r.unwrap_err().to_string().contains("sem"));
    assert!(k.c.anterior.join(EXE).is_file() && k.c.falhou.join(EXE).is_file());
    let _ = restos;
}

/// Operações contadas da preparação; pânico ("queda de energia") quando `vida` chega a 0.
struct Prep {
    vida: std::cell::Cell<Option<u32>>,
    ops: std::cell::Cell<u32>,
    tarefas: std::cell::Cell<bool>,
}

impl Prep {
    fn novo(vida: Option<u32>) -> Self {
        Self {
            vida: vida.into(),
            ops: 0.into(),
            tarefas: false.into(),
        }
    }
    fn tique(&self) {
        self.ops.set(self.ops.get() + 1);
        if let Some(v) = self.vida.get() {
            if v == 0 {
                panic!("queda de energia simulada");
            }
            self.vida.set(Some(v - 1));
        }
    }
}

impl Cenario {
    /// Fluxo completo como no `update_me_suporte`: `abrir_troca` → `atualizar` (scripts,
    /// tarefas e guarda em `antes_de_parar`) → `concluido` se consistente.
    fn rodar_completo(&self, s: &mut Falso, p: &Prep) -> ResultType<Resultado> {
        let c = &self.c;
        let trava = abrir_troca(
            c,
            EXE,
            &mut Abrir {
                ler: &mut ler_seguranca(vec![]),
                criar: &mut |d: &Path| {
                    p.tique();
                    criar_pasta(d)
                },
                travar: &mut |f: &Path| {
                    p.tique();
                    travar_arquivo(f)
                },
                apagar_tarefas: &mut || {
                    p.tique();
                    p.tarefas.set(false)
                },
            },
        )?;
        let extras: Vec<(PathBuf, String)> = vec![];
        let preparo = Preparo {
            origem: &self.origem,
            exe_origem: "rustdesk.exe",
            exe_destino: EXE,
            extras: &extras,
            remover: &[],
            criar_novo: criar_pasta,
        };
        let r = atualizar(
            c,
            &preparo,
            &mut |de: &Path, para: &Path| {
                p.tique();
                copiar_arquivo(de, para)
            },
            &mut |_| {
                p.tique();
                Ok(())
            },
            &mut |e: &Path| {
                p.tique();
                validar_ok(e)
            },
            &mut || {
                assert!(!c.iniciada().exists(), "iniciada antes da recuperação registrada");
                for n in [SCRIPT_RECUPERAR, SCRIPT_GUARDA] {
                    p.tique();
                    gravar_marcador(&c.troca.join(n))?;
                }
                p.tique();
                p.tarefas.set(true);
                p.tique(); // guarda iniciada
                Ok(())
            },
            s,
            true,
            &T0,
        );
        if r.consistente() {
            fs::write(c.concluido(), "ok")?;
            p.tarefas.set(false);
        }
        drop(trava);
        Ok(r)
    }
}

/// Queda em cada ponto da preparação (criar pastas, trava, descarte, cópia, conferências,
/// scripts, tarefas, guarda), com e sem a tarefa rodando a recuperação depois: nada é trocado,
/// a atual continua no lugar e rodando, e a atualização seguinte funciona.
#[test]
fn queda_em_cada_ponto_da_preparacao_nao_bloqueia_a_seguinte() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    // máquina sem `ProgramData\<App>` e máquina com restos de uma atualização concluída
    for com_restos in [false, true] {
        let montar = || {
            let k = Cenario::novo("BOM 1.0.7");
            if com_restos {
                plantar_restos(&k.c, &[MARCADOR_INICIADA, MARCADOR_CONCLUIDO]);
            }
            k
        };
        let total = {
            let k = montar();
            let mut s = Falso::novo(&k.c);
            let p = Prep::novo(None);
            assert_eq!(k.rodar_completo(&mut s, &p).unwrap(), Resultado::Atualizado);
            p.ops.get()
        };
        assert!(total > 12, "{total}");
        for ponto in 0..total {
            for tarefa_roda in [false, true] {
                let k = montar();
                let mut s = Falso::novo(&k.c);
                let p = Prep::novo(Some(ponto));
                let caiu = queda(catch_unwind(AssertUnwindSafe(|| k.rodar_completo(&mut s, &p))));
                let ctx = format!(
                    "com_restos={com_restos} ponto={ponto}/{total} tarefa_roda={tarefa_roda} fase={:?}",
                    k.c.fase()
                );
                assert!(caiu, "{ctx}");
                assert_ne!(k.c.fase(), Fase::TrocaIniciada, "{ctx}");
                assert!(!s.eventos().iter().any(|e| e == "parar"), "parou o serviço: {ctx}");
                k.anterior_intacta_no_lugar();
                if tarefa_roda && p.tarefas.get() {
                    let mut s2 = Falso::novo(&k.c);
                    assert!(recuperar(&k.c, EXE, &mut s2, &T0), "{ctx}");
                    k.anterior_intacta_no_lugar();
                }
                let mut s3 = Falso::novo(&k.c);
                let p3 = Prep::novo(None);
                let r = k.rodar_completo(&mut s3, &p3);
                assert!(matches!(r, Ok(Resultado::Atualizado)), "{ctx}: {r:?}");
                assert_eq!(ler(&k.c.atual.join(EXE)), "BOM 1.0.7", "{ctx}");
                ativo_utilizavel(&k.c, &s3).unwrap_or_else(|e| panic!("{e}: {ctx}"));
                assert!(!p3.tarefas.get(), "{ctx}");
            }
        }
    }
}

/// `iniciada` só depois de scripts/tarefas/guarda e da conferência final; nada disso pronto =
/// sem `iniciada` (preparação incompleta).
#[test]
fn iniciada_so_depois_da_recuperacao_registrada() {
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
    assert!(matches!(r, Resultado::NaoTrocado(_)), "{r:?}");
    assert!(!k.c.iniciada().exists());
    assert_eq!(k.c.fase(), Fase::PreparacaoIncompleta);
    let k = Cenario::novo("BOM 1.0.7");
    let mut s = Falso::novo(&k.c);
    assert_eq!(k.rodar_completo(&mut s, &Prep::novo(None)).unwrap(), Resultado::Atualizado);
    assert!(k.c.iniciada().is_file() && k.c.concluido().is_file());
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

/// Estados em que a recuperação NÃO tem como concluir (R4, item 5). Neles ela fica
/// `Pendente` (tarefas mantidas, sem `concluido`) e tenta de novo a cada minuto e a cada boot;
/// nunca grava `concluido`. São necessários: fora deles, a recuperação sempre conclui.
///  - `servico_quebrado`: o serviço não sobe com nenhuma cópia (SCM/conta/driver).
///  - `nenhuma_boa`: nenhuma das três pastas tem uma instalação que sobe e fica.
///  - `ren_bloqueado`: alguma pasta com `ren` bloqueado sem fim (antivírus/arquivo preso);
///    converge quando o bloqueio some (`recuperacao_repetida_converge…`).
///  - `anterior_que_nao_sobe`: troca interrompida em que a `.anterior` (a versão que rodava
///    antes) não sobe; a recuperação a devolve ao lugar e ela não fica RUNNING. Só acontece se
///    a versão antiga já caía antes da atualização.
///  - `atual_que_nao_sobe_sem_anterior`: sem `.anterior`, a atual tem EXE que não sobe (a boa,
///    se houver, está em `.falhou`). Não é alcançável pela troca: `.falhou` só recebe a nova
///    que não subiu, e a atual é a anterior devolvida.
///  - `concluido_com_atual_que_nao_sobe`: `concluido` já gravado (serviço foi confirmado) e a
///    atual parou de subir depois. Não é alcançável: depois de `concluido` as tarefas saem.
fn motivos_pendente(
    estado: [Option<&str>; 3],
    concluido: bool,
    falhas: [u32; 3],
    quebrado: bool,
) -> Vec<&'static str> {
    let bom = |x: Option<&str>| x == Some("BOM");
    let [a, b, _f] = estado;
    let mut m = vec![];
    if quebrado {
        m.push("servico_quebrado");
    }
    if !estado.iter().any(|x| bom(*x)) {
        m.push("nenhuma_boa");
    }
    if falhas.iter().any(|n| *n >= 99) {
        m.push("ren_bloqueado");
    }
    if b.is_some() && !bom(b) && !(concluido && a.is_some()) {
        m.push("anterior_que_nao_sobe");
    }
    if b.is_none() && a.is_some() && !bom(a) {
        m.push("atual_que_nao_sobe_sem_anterior");
    }
    if concluido && a.is_some() && !bom(a) {
        m.push("concluido_com_atual_que_nao_sobe");
    }
    m
}

/// Todos os estados iniciais (cada pasta ausente/boa/ruim/sobe-e-cai, com e sem `concluido`),
/// todos os padrões de falha de `ren` (0, 1 = passageira, 99 = permanente) por pasta, serviço
/// parado, rodando (só quando a atual sobe: estado coerente) ou quebrado: a invariante vale em
/// todo passo; `Concluida` só com `ativo_utilizavel` (o caminho ativo com instalação que sobe
/// e o serviço rodando ELA), `concluido` gravado e tarefas apagadas; `Pendente` só nos
/// `motivos_pendente`, mantendo as tarefas e sem gravar `concluido`; a anterior boa nunca é
/// destruída; com `concluido` e a atual presente, nada é renomeado.
#[test]
fn recuperacao_exaustiva() {
    let mut casos = 0;
    let mut concluidas = 0;
    let mut por_motivo: BTreeMap<&str, u32> = BTreeMap::new();
    for a in CONTEUDOS {
        for b in CONTEUDOS {
            for f in CONTEUDOS {
                let estado = [a, b, f];
                if estado.iter().all(Option::is_none) {
                    continue; // fora da invariante desde o início
                }
                let atual_sobe = matches!(a, Some("BOM") | Some("CAI"));
                for concluido in [false, true] {
                    for padrao in 0..27usize {
                        let falhas = [padrao % 3, padrao / 3 % 3, padrao / 9].map(|i| [0, 1, 99][i]);
                        // R5: `.falhou` que o `rd` não apaga (arquivo preso)
                        for presa in [false, true] {
                        for (inicial, quebrado) in [
                            (Servico::Rodando, false),
                            (Servico::Parado, false),
                            (Servico::Parado, true),
                        ] {
                            if inicial == Servico::Rodando && !atual_sobe {
                                continue; // RUNNING com EXE que não sobe: estado impossível
                            }
                            casos += 1;
                            let m = Mini::novo(estado, concluido);
                            let mut s = Falso::novo(&m.c);
                            assert_eq!(s.estado, if atual_sobe { Servico::Rodando } else { Servico::Parado });
                            s.estado = inicial;
                            s.quebrado = quebrado;
                            s.falhou_presa = presa;
                            for (p, n) in Pasta::TODAS.iter().zip(falhas) {
                                if n > 0 {
                                    s.falhas_renomear.insert(m.c.pasta(*p).to_owned(), n);
                                }
                            }
                            let anterior = ler(&m.c.anterior.join(EXE));
                            let saida = recuperar_conferindo(&m.c, &mut s);
                            let motivos = motivos_pendente(estado, concluido, falhas, quebrado);
                            let ctx = format!(
                                "{estado:?} concluido={concluido} falhas={falhas:?} {inicial:?} \
                                 quebrado={quebrado} presa={presa} -> {saida:?} {motivos:?} {:?}",
                                s.eventos()
                            );
                            match saida {
                                Saida::Concluida => {
                                    concluidas += 1;
                                    if let Err(e) = ativo_utilizavel(&m.c, &s) {
                                        panic!("concluiu sem instalação utilizável no caminho ativo: {e}: {ctx}");
                                    }
                                    assert!(m.c.concluido().is_file(), "{ctx}");
                                    assert!(!s.tarefas, "{ctx}");
                                }
                                Saida::Ocupada => unreachable!("corpo não sai ocupado"),
                                Saida::Pendente => {
                                    assert!(!motivos.is_empty(), "pendente sem motivo conhecido: {ctx}");
                                    for x in &motivos {
                                        *por_motivo.entry(x).or_default() += 1;
                                    }
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
                                assert!(!s.eventos().iter().any(|e| e.starts_with("ren:")), "{ctx}");
                            }
                            if presa && f.is_some() && saida == Saida::Concluida && b.is_some() {
                                // a `.falhou` presa saiu do caminho (afastada) em vez de travar
                                assert!(!s.eventos().iter().any(|e| e == "ren-destino-existe"), "{ctx}");
                            }
                        }
                        }
                    }
                }
            }
        }
    }
    // (63 estados × 2 (concluido) × 27 (falhas) × (parado + quebrado) + 32 estados com a atual
    // que sobe × 2 × 27 rodando) × 2 (`.falhou` presa ou não)
    assert_eq!(casos, (63 * 2 * 27 * 2 + 32 * 2 * 27) * 2);
    assert!(concluidas > 0 && concluidas < casos);
    eprintln!("recuperacao_exaustiva: {casos} casos, {concluidas} concluídas, pendentes por motivo {por_motivo:?}");
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
            ativo_utilizavel(&m.c, &s).unwrap_or_else(|e| panic!("{e}: {ctx}"));
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
                ativo_utilizavel(&k.c, &s3).unwrap_or_else(|e| panic!("{e}: {ctx}"));
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
const RELIGAR: &str = "SuporteVancin-recuperar-atualizacao-religar";
const TF: Tarefas = Tarefas {
    fixas: &TAREFAS,
    religar: RELIGAR,
};
const TC: Tarefas = Tarefas {
    fixas: &["C"],
    religar: "R",
};

fn bat() -> String {
    script_recuperar(
        &caminhos_windows(),
        "SuporteVancin",
        "RuntimeBroker_rustdesk.exe",
        &TF,
        &Tempos::PADRAO,
    )
    .unwrap()
}

/// Linhas de `:inicio` até o próximo rótulo de modo (`:manual`, `:religar`, `:principal`).
fn secao<'a>(ls: &[&'a str], inicio: &str) -> Vec<&'a str> {
    let i = ls.iter().position(|l| *l == inicio).unwrap_or_else(|| panic!("sem {inicio}"));
    ls[i + 1..]
        .iter()
        .take_while(|l| ![":manual", ":religar", ":principal", ":ren"].contains(*l))
        .copied()
        .collect()
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
    let m = Modos::padrao();
    validar_modos(&m).unwrap();
    for p in m.automatico.iter().chain(&m.manual).chain(&m.religar).chain(&m.corpo) {
        if let Passo::Rotulo(r) = p {
            assert!(rotulos.contains(r), "{r}");
        }
    }
    assert!(!ls.iter().any(|l| l.trim_start().starts_with("move ")));
    assert!(b.contains(r#"set "ATUAL=C:/Program Files/SuporteVancin""#));
    assert!(b.contains(r#"set "TROCA=C:/ProgramData/SuporteVancin/atualizacao""#));
}

/// Recuperação serializada: o corpo só roda com o descritor 9 em `recuperar.lock`, em todos
/// os modos; sem a trava, `SAIDA` continua 3 e nada é tocado. `:principal` não é alcançável
/// por queda; fora dele nada mexe em serviço, pasta, processo ou apaga tarefa.
#[test]
fn bat_corpo_so_roda_com_a_trava() {
    let b = bat();
    let ls = linhas(&b);
    let corpo = ls.iter().position(|l| *l == ":principal").unwrap();
    let chamadas: Vec<usize> = (0..ls.len()).filter(|i| ls[*i].contains(":principal")).collect();
    // automático, manual e religar: uma chamada cada (+ o rótulo)
    assert_eq!(chamadas.len(), 4, "{chamadas:?}");
    for i in chamadas.iter().filter(|i| **i != corpo) {
        assert_eq!(ls[*i], r#"call :principal 9>"%TROCA%\recuperar.lock""#);
        assert_eq!(ls[i - 1], r#"set "SAIDA=3""#);
        assert_eq!(ls[i + 1], r#"if "%SAIDA%"=="0" goto "#.to_owned() + ls[i + 1].rsplit(' ').next().unwrap());
        assert!(ls[i + 2].starts_with(r#"if "%SAIDA%"=="1" goto "#), "{}", ls[i + 2]);
    }
    assert!(ls[corpo - 1].starts_with("exit /b "), "queda para dentro de :principal");
    for l in &ls[..corpo] {
        for proibido in ["sc ", "ren ", "rd ", "taskkill", "schtasks /Delete", "echo ok>"] {
            assert!(!l.contains(proibido), "{l}");
        }
    }
    // modo desconhecido não roda nada
    let desp = linha(&b, r#"if not "%~1"=="" exit /b 2"#);
    assert_eq!(ls[desp - 2], r#"if /i "%~1"=="manual" goto manual"#);
    assert_eq!(ls[desp - 1], r#"if /i "%~1"=="religar" goto religar"#);
    // automático: trava ocupada = log e sai com 3
    let auto = secao(&ls, r#"if not "%~1"=="" exit /b 2"#);
    assert_eq!(auto[4], "goto auto_ocupado");
    let oc = auto.iter().position(|l| *l == ":auto_ocupado").unwrap();
    assert!(auto[oc + 1].contains("recuperar ocupado"));
    assert_eq!(auto[oc + 2], "exit /b 3");
}

/// R5, achado 1: o modo manual (1) registra `religar`, (2) desabilita as duas tarefas, (3) só
/// então pega a trava (esperando), (4) não apaga tarefa nem mexe em pasta/serviço fora do
/// corpo e (5) reabilita as tarefas em todo caminho que não seja `Concluida`.
#[test]
fn bat_manual_desabilita_espera_a_trava_e_reabilita_na_falha() {
    let b = bat();
    let ls = linhas(&b);
    let m = secao(&ls, ":manual");
    let pos = |t: &str| {
        m.iter()
            .position(|l| l.contains(t))
            .unwrap_or_else(|| panic!("manual sem {t}:\n{}", m.join("\n")))
    };
    let criar = pos("schtasks /Create");
    assert_eq!(
        m[criar],
        format!(
            r#"schtasks /Create /F /RU SYSTEM /RL HIGHEST /SC ONSTART /TN "{RELIGAR}" /TR "\"%TROCA%\recuperar.bat\" religar" >nul 2>&1 || goto manual_sem_religar"#
        )
    );
    let mut desab = vec![];
    for tn in TAREFAS {
        let d = pos(&format!(r#"schtasks /Change /TN "{tn}" /DISABLE"#));
        let rot = m[d].rsplit(' ').next().unwrap();
        // não desabilitou: só segue se a tarefa não existe mais
        assert_eq!(m[d + 1], format!(r#"schtasks /Query /TN "{tn}" >nul 2>&1 && goto manual_reabilitar"#));
        assert_eq!(m[d + 2], format!(":{rot}"));
        desab.push(d);
    }
    let chama = pos("call :principal");
    assert!(criar < desab[0] && desab[0] < desab[1] && desab[1] < chama);
    // espera a trava (400 × 3 s) e desiste reabilitando
    assert_eq!(m[chama + 1], r#"if "%SAIDA%"=="0" goto manual_ok"#);
    assert_eq!(m[chama + 2], r#"if "%SAIDA%"=="1" goto manual_reabilitar"#);
    assert_eq!(m[chama + 3], "if %T% GEQ 400 goto manual_reabilitar");
    assert_eq!(m[chama - 2], m[chama + 6].replace("goto ", ":"), "laço da espera");
    // fora do corpo: nada de apagar tarefa, nem serviço/pasta
    for l in &m {
        for proibido in ["schtasks /Delete", "sc ", "ren ", "rd ", "taskkill", "/ENABLE >nul 2>&1 && goto manual_ok"] {
            assert!(!l.contains(proibido), "{l}");
        }
    }
    // todo caminho que não é `Concluida` passa pela reabilitação antes de sair
    let reab = pos(":manual_reabilitar");
    for (j, tn) in TAREFAS.iter().enumerate() {
        assert!(m[reab + 1 + 3 * j].starts_with(&format!(r#"schtasks /Change /TN "{tn}" /ENABLE"#)));
    }
    let ok = pos(":manual_ok");
    assert!(m[ok..reab].iter().any(|l| *l == "exit /b 0"));
    assert!(m[reab..].iter().any(|l| *l == "exit /b 1"));
    // `manual_ok` só vem do corpo `Concluida` (SAIDA=0)
    let para_ok: Vec<&&str> = ls.iter().filter(|l| l.ends_with("goto manual_ok")).collect();
    assert_eq!(para_ok, vec![&r#"if "%SAIDA%"=="0" goto manual_ok"#]);
    // religar: reabilita antes de pegar a trava
    let r = secao(&ls, ":religar");
    let en = r.iter().position(|l| l.contains("/ENABLE")).unwrap();
    let ch = r.iter().position(|l| l.contains("call :principal")).unwrap();
    assert!(en < ch);
    assert_eq!(r[ch + 3], "goto religar_ocupado");
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
    assert_eq!(alvos.len(), 4, "2 tarefas fixas + religar + concluido");
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
        .filter(|p| matches!(p, Passo::Renomear { .. } | Passo::Afastar { .. }))
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
        trava: 11,
    };
    let b = script_recuperar(&caminhos_windows(), "A", "B", &TC, &t).unwrap();
    for trecho in [
        "if %T% GEQ 11 goto manual_reabilitar",
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
        assert!(script_recuperar(&c, "A", "B", &TC, &T0).is_err(), "{ruim}");
        let c = Caminhos::de(Path::new("C:/ok"), Path::new(ruim));
        assert!(script_guarda(&c, 1, 1).is_err(), "{ruim}");
        assert!(script_recuperar(&c, "A", "B", &TC, &T0).is_err(), "{ruim}");
    }
    let c = caminhos_windows();
    assert!(script_recuperar(&c, "A&B", "B", &TC, &T0).is_err());
    let ruim = Tarefas { fixas: &["C|D"], religar: "R" };
    assert!(script_recuperar(&c, "A", "B", &ruim, &T0).is_err());
    let ruim = Tarefas { fixas: &["C"], religar: "R&S" };
    assert!(script_recuperar(&c, "A", "B", &ruim, &T0).is_err());
}

// ------------------------------------------------------------------------------------
// R5, achado 1: recuperação manual (`recuperar.bat manual`) disputando com a guarda e com as
// tarefas, `.falhou` pré-existente e queda em cada passo.
// ------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ator {
    Manual,
    Minuto,
    Guarda,
}

#[derive(Default, Debug)]
struct Disputa {
    saidas: Vec<(String, Saida)>,
    /// O manual tentou a trava e a achou ocupada (esperou outra execução terminar).
    manual_esperou: bool,
    /// Uma execução automática achou a trava ocupada pelo manual (saiu com 3).
    auto_barrada: bool,
}

/// Escalonador: a tarefa de minuto já está rodando no passo 0; o técnico roda o manual no
/// passo `d_manual`; a guarda entra no passo `d_guarda`; a tarefa de minuto dispara a cada
/// `cada` passos se existe e está habilitada (uma instância por vez, como no Agendador); cada
/// ator roda `q` passos por vez. Erro = invariante violada (sem pânico, para a prova de
/// mutação).
fn disputar(
    m: &Mini,
    s: &mut Falso,
    d_manual: usize,
    d_guarda: Option<usize>,
    cada: usize,
    q: usize,
) -> Result<Disputa, String> {
    let corpo = programa_recuperar();
    let (auto, manual) = (programa_automatico(), programa_manual());
    let mut atores: Vec<(Ator, Exec)> = vec![(Ator::Minuto, Exec::novo(&auto, &corpo))];
    let (mut lancou_manual, mut lancou_guarda) = (false, d_guarda.is_none());
    let mut d = Disputa::default();
    let (mut passo, mut vez) = (0usize, 0usize);
    loop {
        if !lancou_manual && passo >= d_manual {
            atores.push((Ator::Manual, Exec::novo(&manual, &corpo)));
            lancou_manual = true;
        }
        if !lancou_guarda && passo >= d_guarda.unwrap() {
            atores.push((Ator::Guarda, Exec::novo(&auto, &corpo)));
            lancou_guarda = true;
        }
        if passo > 0
            && passo % cada == 0
            && s.tarefas
            && s.habilitadas
            && !atores.iter().any(|a| a.0 == Ator::Minuto)
        {
            atores.push((Ator::Minuto, Exec::novo(&auto, &corpo)));
        }
        if atores.is_empty() {
            if lancou_manual && lancou_guarda {
                break;
            }
            passo += 1;
            continue;
        }
        if passo > 50_000 {
            return Err("não terminou".into());
        }
        vez %= atores.len();
        let mut fim = None;
        for _ in 0..q {
            let manual_dentro = atores.iter().any(|a| a.0 == Ator::Manual && a.1.no_corpo());
            let antes = s.eventos.borrow().len();
            let (quem, e) = &mut atores[vez];
            let (i, p, x) = e.avancar(&m.c, EXE, s, &T0);
            passo += 1;
            let ocupada = s.eventos.borrow()[antes..].iter().any(|e| e == "trava-ocupada");
            if ocupada && *quem == Ator::Manual {
                d.manual_esperou = true;
            }
            if ocupada && *quem != Ator::Manual && manual_dentro {
                d.auto_barrada = true;
            }
            if s.donos > 1 {
                return Err(format!("dois corpos ao mesmo tempo ({quem:?} passo {i} {p:?})"));
            }
            if !alguma_utilizavel(&m.c) {
                return Err(format!("sem instalação utilizável ({quem:?} passo {i} {p:?})"));
            }
            if let Some(x) = x {
                fim = Some(x);
                break;
            }
        }
        if let Some(x) = fim {
            let (quem, _) = atores.remove(vez);
            d.saidas.push((format!("{quem:?}"), x));
        } else {
            vez += 1;
        }
    }
    if s.donos != 0 {
        return Err("trava não solta".into());
    }
    Ok(d)
}

/// Estados de troca interrompida em que a recuperação tem como concluir (sem `motivos`).
const INTERROMPIDAS: [[Option<&str>; 3]; 4] = [
    [Some("RUIM"), Some("BOM"), None],
    [None, Some("BOM"), None],
    [Some("CAI"), Some("BOM"), Some("RUIM")],
    [Some("RUIM"), Some("BOM"), Some("BOM")],
];

/// Manual × tarefa de minuto × guarda, em todo deslocamento de início do manual e em vários
/// quanta: nunca dois corpos ao mesmo tempo; a invariante vale em todo passo; o manual termina
/// `Concluida` (esperando a execução em curso quando preciso); nenhuma automática fica
/// `Pendente`; ao fim, caminho ativo utilizável, `concluido`, tarefas e `religar` apagadas.
#[test]
fn manual_disputa_com_tarefa_de_minuto_e_guarda() {
    let (mut casos, mut esperou, mut barrada) = (0, 0, 0);
    for estado in INTERROMPIDAS {
        assert!(motivos_pendente(estado, false, [0; 3], false).is_empty());
        for d_manual in 0..36 {
            for d_guarda in [None, Some(0), Some(7), Some(23)] {
                for cada in [3, 8] {
                    for q in [1, 2, 5] {
                        casos += 1;
                        let m = Mini::novo(estado, false);
                        let mut s = Falso::novo(&m.c);
                        let ctx = format!("{estado:?} manual@{d_manual} guarda@{d_guarda:?} cada={cada} q={q}");
                        let d = disputar(&m, &mut s, d_manual, d_guarda, cada, q)
                            .unwrap_or_else(|e| panic!("{e}: {ctx} {:?}", s.eventos()));
                        let ctx = format!("{ctx} {:?} {:?}", d.saidas, s.eventos());
                        let manual: Vec<_> = d.saidas.iter().filter(|x| x.0 == "Manual").collect();
                        assert_eq!(manual, vec![&("Manual".to_owned(), Saida::Concluida)], "{ctx}");
                        assert!(!d.saidas.iter().any(|x| x.1 == Saida::Pendente), "{ctx}");
                        ativo_utilizavel(&m.c, &s).unwrap_or_else(|e| panic!("{e}: {ctx}"));
                        assert!(m.c.concluido().is_file(), "{ctx}");
                        assert!(!s.tarefas && !s.religar, "{ctx}");
                        esperou += d.manual_esperou as u32;
                        barrada += d.auto_barrada as u32;
                    }
                }
            }
        }
    }
    // os dois lados da disputa aconteceram de fato
    assert!(esperou > 0 && barrada > 0, "esperou={esperou} barrada={barrada}");
    eprintln!("manual_disputa: {casos} casos, manual esperou em {esperou}, automática barrada em {barrada}");
}

/// Prova de mutação: com a trava sem efeito, o mesmo escalonador acha dois corpos ao mesmo
/// tempo (o teste acima enxergaria a falta da trava).
#[test]
fn manual_sem_trava_e_detectado() {
    let mut detectados = 0;
    let mut casos = 0;
    for estado in INTERROMPIDAS {
        for d_manual in 0..12 {
            casos += 1;
            let m = Mini::novo(estado, false);
            let mut s = Falso::novo(&m.c);
            s.trava_quebrada = true;
            if let Err(e) = disputar(&m, &mut s, d_manual, Some(0), 3, 1) {
                assert!(e.starts_with("dois corpos"), "{e}");
                detectados += 1;
            }
        }
    }
    assert!(detectados * 2 > casos, "{detectados}/{casos}");
}

fn manual(m: &Mini, s: &mut Falso) -> Saida {
    executar(&programa_manual(), &m.c, EXE, s, &T0, &mut |i, p| {
        assert!(alguma_utilizavel(&m.c), "passo {i} ({p:?}) deixou sem instalação utilizável");
    })
}

/// `.falhou` pré-existente que o `rd` não apaga (arquivo preso): a tabela não trava no
/// destino existente; afasta para o primeiro `.falhou.<n>` livre e conclui. Sem nome livre ou
/// com o `ren` dela bloqueado: `Pendente`, nada perdido, tarefas reabilitadas.
#[test]
fn falhou_preexistente_presa_e_afastada() {
    for ocupadas in [0, 1, 4, AFASTADAS] {
        let m = Mini::novo([Some("RUIM"), Some("BOM"), Some("RUIM")], false);
        for n in 1..=ocupadas {
            escrever(&m.c.afastada(n).join(EXE), &format!("RUIM antiga {n}"));
        }
        let mut s = Falso::novo(&m.c);
        s.falhou_presa = true;
        let saida = manual(&m, &mut s);
        let ctx = format!("ocupadas={ocupadas} {saida:?} {:?}", s.eventos());
        assert!(s.eventos().contains(&"rd-preso".to_owned()), "{ctx}");
        assert!(!s.eventos().contains(&"ren-destino-existe".to_owned()), "{ctx}");
        if ocupadas < AFASTADAS {
            assert_eq!(saida, Saida::Concluida, "{ctx}");
            ativo_utilizavel(&m.c, &s).unwrap_or_else(|e| panic!("{e}: {ctx}"));
            assert_eq!(ler(&m.c.atual.join(EXE)), "BOM Anterior", "{ctx}");
            assert_eq!(ler(&m.c.afastada(ocupadas + 1).join(EXE)), "RUIM Falhou", "{ctx}");
            assert_eq!(ler(&m.c.falhou.join(EXE)), "RUIM Atual", "{ctx}");
            assert!(!s.tarefas && !s.religar, "{ctx}");
        } else {
            assert_eq!(saida, Saida::Pendente, "{ctx}");
            assert_eq!(ler(&m.c.anterior.join(EXE)), "BOM Anterior", "{ctx}");
            assert!(s.tarefas && s.habilitadas && !m.c.concluido().exists(), "{ctx}");
        }
        for n in 1..=ocupadas {
            assert_eq!(ler(&m.c.afastada(n).join(EXE)), format!("RUIM antiga {n}"), "{ctx}");
        }
    }
    // presa e com `ren` bloqueado: não sobrescreve nada, fica pendente com as tarefas ativas
    let m = Mini::novo([Some("RUIM"), Some("BOM"), Some("RUIM")], false);
    let mut s = Falso::novo(&m.c);
    s.falhou_presa = true;
    s.falhas_renomear.insert(m.c.falhou.clone(), 99);
    assert_eq!(manual(&m, &mut s), Saida::Pendente);
    assert_eq!(ler(&m.c.anterior.join(EXE)), "BOM Anterior");
    assert_eq!(ler(&m.c.falhou.join(EXE)), "RUIM Falhou");
    assert!(s.tarefas && s.habilitadas);
}

/// Falhas do manual fora do corpo: sem `religar` não mexe em nada; não desabilitou, trava que
/// não vem ou corpo pendente = tarefas reabilitadas (a automática retoma) e `Pendente`.
#[test]
fn manual_que_falha_reabilita_as_tarefas() {
    let estado = [Some("RUIM"), Some("BOM"), None];
    let pastas = |m: &Mini| Pasta::TODAS.map(|p| ler(&m.c.pasta(p).join(EXE)));
    for caso in ["criar_religar", "desabilitar", "trava_presa", "corpo_pendente"] {
        let m = Mini::novo(estado, false);
        let antes = pastas(&m);
        let mut s = Falso::novo(&m.c);
        match caso {
            "criar_religar" | "desabilitar" => s.falhar_agenda.push(caso),
            "trava_presa" => s.donos = 1, // outra execução segura a trava o tempo todo
            _ => s.quebrado = true,
        }
        let saida = manual(&m, &mut s);
        let ev = s.eventos();
        let ctx = format!("{caso}: {saida:?} {ev:?}");
        assert_eq!(saida, Saida::Pendente, "{ctx}");
        assert!(s.tarefas && s.habilitadas, "{ctx}");
        assert!(!m.c.concluido().exists(), "{ctx}");
        match caso {
            "criar_religar" => {
                assert!(!s.religar && !ev.iter().any(|e| e == "desabilitar" || e == "habilitar"), "{ctx}");
            }
            "trava_presa" => {
                let n = ev.iter().filter(|e| *e == "trava-ocupada").count() as u32;
                assert_eq!(n, T0.trava + 1, "{ctx}");
            }
            _ => {}
        }
        if caso != "corpo_pendente" {
            assert_eq!(pastas(&m), antes, "{ctx}");
            assert!(!ev.iter().any(|e| e == "parar" || e.starts_with("ren")), "{ctx}");
        }
        if caso != "criar_religar" {
            // `habilitar` depois de `desabilitar` (quando houve)
            let h = ev.iter().rposition(|e| e == "habilitar").expect("reabilitou");
            assert!(ev.iter().position(|e| e == "desabilitar").map_or(true, |d| d < h), "{ctx}");
        }
    }
}

/// Queda de energia (ou janela fechada) em cada operação do manual. Depois do boot: a
/// `religar` (se registrada) reabilita as tarefas, a do boot e a de minuto rodam o automático;
/// sem tarefa nenhuma, o SCM sobe o serviço (início automático). Sem boot: o técnico roda o
/// manual de novo. Em todos: caminho ativo utilizável, `concluido`, tarefas e `religar`
/// apagadas; a invariante vale em todo passo.
#[test]
fn queda_em_cada_passo_da_recuperacao_manual() {
    use std::panic::{catch_unwind, AssertUnwindSafe};
    let corpo = programa_recuperar();
    let _ = corpo;
    for estado in INTERROMPIDAS {
        let total = {
            let m = Mini::novo(estado, false);
            let mut s = Falso::novo(&m.c);
            s.vida = Some(u32::MAX);
            assert_eq!(manual(&m, &mut s), Saida::Concluida);
            u32::MAX - s.vida.unwrap()
        };
        for k in 0..total {
            for reboot in [true, false] {
                let m = Mini::novo(estado, false);
                let mut s = Falso::novo(&m.c);
                s.vida = Some(k);
                let caiu = queda(catch_unwind(AssertUnwindSafe(|| manual(&m, &mut s))));
                assert!(caiu, "{estado:?} k={k}");
                assert!(alguma_utilizavel(&m.c), "{estado:?} k={k}");
                let ctx = format!(
                    "{estado:?} k={k}/{total} reboot={reboot} tarefas={} habilitadas={} religar={}",
                    s.tarefas, s.habilitadas, s.religar
                );
                if reboot {
                    let mut s2 = Falso::novo(&m.c);
                    s2.estado = Servico::Parado;
                    (s2.tarefas, s2.habilitadas, s2.religar) = (s.tarefas, s.habilitadas, s.religar);
                    let mut saidas = vec![];
                    let mut roda = |prog: Vec<Passo>, s2: &mut Falso| {
                        let x = executar(&prog, &m.c, EXE, s2, &T0, &mut |i, p| {
                            assert!(alguma_utilizavel(&m.c), "passo {i} {p:?}");
                        });
                        saidas.push(x);
                    };
                    if s2.religar {
                        roda(programa_religar(), &mut s2);
                    }
                    for _ in 0..3 {
                        // ao iniciar e depois a de minuto
                        if s2.tarefas && s2.habilitadas {
                            roda(programa_automatico(), &mut s2);
                        }
                    }
                    if !s2.tarefas && !s2.religar && s2.estado != Servico::Rodando {
                        s2.iniciar_servico(); // SCM: serviço com início automático
                    }
                    let ctx = format!("{ctx} {saidas:?} {:?}", s2.eventos());
                    ativo_utilizavel(&m.c, &s2).unwrap_or_else(|e| panic!("{e}: {ctx}"));
                    assert!(m.c.concluido().is_file(), "{ctx}");
                    assert!(!s2.tarefas && !s2.religar, "{ctx}");
                } else {
                    // o processo morreu: a trava sai com ele; o técnico roda de novo
                    s.vida = None;
                    s.donos = 0;
                    let saida = manual(&m, &mut s);
                    let ctx = format!("{ctx} {saida:?} {:?}", s.eventos());
                    assert_eq!(saida, Saida::Concluida, "{ctx}");
                    ativo_utilizavel(&m.c, &s).unwrap_or_else(|e| panic!("{e}: {ctx}"));
                    assert!(!s.tarefas && !s.religar, "{ctx}");
                }
            }
        }
    }
}

#[test]
fn modos_invalidos_sao_recusados() {
    use Passo::*;
    validar_modos(&Modos::padrao()).unwrap();
    // corpo não pode mexer na agenda nem pegar a trava de novo
    let mut m = Modos::padrao();
    m.corpo.insert(0, HabilitarTarefas);
    assert!(validar_modos(&m).is_err());
    // modo não pode mexer em pasta/serviço fora da trava
    for p in [PararServico, Apagar(Pasta::Falhou), ApagarTarefas, SubirEstavel { falha: "manual_ok" }] {
        let mut m = Modos::padrao();
        m.manual.insert(0, p);
        assert!(validar_modos(&m).is_err(), "{p:?}");
    }
    // rótulo repetido entre programas / reservado
    let mut m = Modos::padrao();
    m.manual.insert(0, Rotulo("garantir"));
    assert!(validar_modos(&m).is_err());
    let mut m = Modos::padrao();
    m.manual.insert(0, Rotulo("principal"));
    assert!(validar_modos(&m).is_err());
}

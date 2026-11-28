//! Troca da instalação do Windows com volta automática (app de suporte).
//!
//! Garantia (R4, sem exagero): o acesso remoto fica fora do ar só durante a troca (serviço
//! parado → pastas renomeadas → serviço de volta, em geral segundos). Se a nova não sobe, a
//! anterior volta sozinha. Se o processo morre ou a máquina desliga no meio, a guarda e as
//! tarefas SYSTEM repetem a recuperação a cada boot e a cada minuto até o serviço ficar
//! RUNNING estável no caminho instalado. O que NÃO dá para garantir: (1) prazo, se o `ren`
//! ficar bloqueado para sempre (antivírus/arquivo preso) ou se o serviço não subir com
//! nenhuma das cópias, a recuperação continua tentando e a máquina fica sem acesso remoto até
//! alguém agir no local; (2) a sessão de quem está conectado cai na troca e precisa
//! reconectar (uma sessão aberta depois da checagem também cai). ID, senha permanente e
//! configuração (perfil do serviço, fora da pasta de instalação) não são tocados.
//!
//! Desenho (pastas irmãs da instalação, no mesmo volume, para `rename` atômico):
//!   `<inst>.novo`      nova instalação montada e conferida ANTES de parar o serviço
//!   `<inst>.anterior`  instalação anterior, íntegra (é só renomeada, nunca sobrescrita)
//!   `<inst>.falhou`    nova instalação que não subiu (fica para diagnóstico)
//! e, fora da pasta de programas (`%ProgramData%\<App>\atualizacao`, DACL protegida):
//!   `troca`            scripts da guarda/recuperação, trava, log e o marcador `concluido`
//!
//! Só atualiza quando a instalação, a pasta-mãe e os ancestrais estão protegidos (`acl`).
//!
//! 0. `abrir_troca` (R4): confere a cadeia existente de `troca` (dono, DACL, reparse em cada
//!    componente) ANTES de ler marcadores ou apagar restos, cria o que faltar já protegido,
//!    pega a trava e só então decide pela fase: `iniciada` sem `concluido` = troca iniciada
//!    (é da recuperação; recusa sem tocar em nada); qualquer outra = preparação incompleta
//!    ou troca concluída, descartada sob a trava (nunca toca a instalação atual).
//!    `iniciada` é gravado só depois de a recuperação estar registrada (scripts, tarefas e
//!    guarda) e conferida, imediatamente antes de parar o serviço.
//!
//! 1. `preparar`: `<inst>.novo` = cópia da instalação atual + arquivos da versão nova por cima
//!    (mesma semântica do XCOPY antigo, que preservava arquivos extras), com sha256 de cada
//!    arquivo conferido contra a origem. Qualquer erro apaga `.novo`; o serviço não foi parado.
//! 2. O chamador confere o EXE montado (`--suporte-build-info`: carrega EXE + DLLs e devolve
//!    marca/variante/versão compiladas). EXE inválido = nada é parado.
//! 3. `trocar`: para o serviço → `inst` → `.anterior` → `.novo` → `inst` → sobe o serviço e
//!    exige que fique `RUNNING` por um tempo. Qualquer falha devolve a anterior e sobe o
//!    serviço dela.
//! 4. Fora do processo (Windows): uma guarda (`guarda.bat`) espera ESTE processo terminar e só
//!    então, se não houver `concluido`, roda `recuperar.bat`. Duas tarefas agendadas (SYSTEM,
//!    "ao iniciar" e "a cada minuto") rodam o mesmo `recuperar.bat`. Uma trava de arquivo
//!    (`recuperar.lock`, também segurada por este processo durante toda a troca) impede duas
//!    execuções ao mesmo tempo. As tarefas só são apagadas depois de o serviço ficar RUNNING;
//!    até lá, a recuperação é repetida.
//! 5. A lógica do `recuperar.bat` é um programa (`programa_recuperar`, lista de `Passo`) com
//!    dois consumidores: `executar` (interpretador sobre `Sistema` e pastas reais, usado nos
//!    testes) e `script_recuperar` (gera o BAT, passo a passo). Os testes exercitam a lógica
//!    que vira BAT, e não o texto dele.
//! 6. R5: o mesmo `recuperar.bat` tem três modos, todos gerados de programas (`Passo`) e
//!    interpretados nos testes: sem argumento (guarda e tarefas: pega a trava ou sai com 3),
//!    `manual` (técnico: registra a tarefa `religar`, desabilita as duas tarefas, ESPERA a trava
//!    e roda o mesmo corpo; falha = reabilita as tarefas) e `religar` (boot depois de uma
//!    recuperação manual interrompida: reabilita as tarefas e roda o automático).

pub(crate) mod acl;

use hbb_common::{anyhow::anyhow, bail, log, ResultType};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) const MARCADOR_CONCLUIDO: &str = "concluido";
pub(crate) const TRAVA: &str = "recuperar.lock";
pub(crate) const SCRIPT_RECUPERAR: &str = "recuperar.bat";
pub(crate) const SCRIPT_GUARDA: &str = "guarda.bat";
/// Gravado com a recuperação já registrada, logo antes de parar o serviço (R4, achado 2).
pub(crate) const MARCADOR_INICIADA: &str = "iniciada";
/// Nomes `<inst>.falhou.1..=AFASTADAS` para um `.falhou` que não pôde ser apagado (R5).
pub(crate) const AFASTADAS: u32 = 9;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Caminhos {
    pub atual: PathBuf,
    pub novo: PathBuf,
    pub anterior: PathBuf,
    pub falhou: PathBuf,
    pub troca: PathBuf,
}

impl Caminhos {
    /// `troca`: pasta da recuperação, fora da pasta de programas
    /// (`%ProgramData%\<App>\atualizacao` no Windows).
    pub fn de(atual: &Path, troca: &Path) -> Self {
        let irmao = |suf: &str| {
            let mut s = atual.as_os_str().to_owned();
            s.push(suf);
            PathBuf::from(s)
        };
        Self {
            atual: atual.to_owned(),
            novo: irmao(".novo"),
            anterior: irmao(".anterior"),
            falhou: irmao(".falhou"),
            troca: troca.to_owned(),
        }
    }

    pub fn concluido(&self) -> PathBuf {
        self.troca.join(MARCADOR_CONCLUIDO)
    }

    pub fn pasta(&self, p: Pasta) -> &Path {
        match p {
            Pasta::Atual => &self.atual,
            Pasta::Anterior => &self.anterior,
            Pasta::Falhou => &self.falhou,
        }
    }

    /// `<inst>.falhou.<n>`: para onde `Afastar` move um `.falhou` que não pôde ser apagado.
    pub fn afastada(&self, n: u32) -> PathBuf {
        let mut s = self.falhou.as_os_str().to_owned();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    pub fn iniciada(&self) -> PathBuf {
        self.troca.join(MARCADOR_INICIADA)
    }

    /// Fase da última atualização, pelos marcadores. Só tem valor depois de a cadeia de
    /// `troca` estar conferida (`abrir_troca`).
    pub fn fase(&self) -> Fase {
        if fs::symlink_metadata(&self.troca).is_err() {
            Fase::Livre
        } else if self.concluido().exists() {
            Fase::Concluida
        } else if self.iniciada().exists() {
            Fase::TrocaIniciada
        } else {
            Fase::PreparacaoIncompleta
        }
    }

    /// A troca começou e não terminou: a recuperação (guarda/tarefas) é dona das pastas.
    pub fn recuperacao_pendente(&self) -> bool {
        self.fase() == Fase::TrocaIniciada
    }
}

/// Ver `Caminhos::fase`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fase {
    /// Sem pasta `troca`.
    Livre,
    /// `troca` sem `iniciada` nem `concluido`: caiu antes de parar o serviço. Nada foi
    /// trocado; pode ser descartada.
    PreparacaoIncompleta,
    /// `iniciada` sem `concluido`: a troca pode ter começado; só a recuperação mexe.
    TrocaIniciada,
    /// `concluido`: serviço confirmado RUNNING depois da última troca.
    Concluida,
}

/// O que `abrir_troca` precisa do sistema. No Windows: `acl::ler_descritor`,
/// `acl::criar_pasta_protegida`, arquivo aberto com `share_mode(0)` e `schtasks /Delete`.
pub(crate) struct Abrir<'a, T> {
    pub ler: &'a mut dyn FnMut(&Path) -> io::Result<acl::Descritor>,
    /// Cria uma pasta já protegida; falha se existir.
    pub criar: &'a mut dyn FnMut(&Path) -> io::Result<()>,
    /// Trava exclusiva (a mesma que o `recuperar.bat` tenta pegar); erro = ocupada.
    pub travar: &'a mut dyn FnMut(&Path) -> io::Result<T>,
    pub apagar_tarefas: &'a mut dyn FnMut(),
}

fn cadeia(
    ler: &mut dyn FnMut(&Path) -> io::Result<acl::Descritor>,
    p: &Path,
    estritos: usize,
) -> ResultType<()> {
    acl::conferir_cadeia(p, estritos, ler).map_err(|e| {
        log::error!("[portal-update] pasta não protegida, não atualizo: {e}");
        anyhow!("pasta não protegida: {e}")
    })
}

fn existe(p: &Path) -> ResultType<bool> {
    match fs::symlink_metadata(p) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => bail!("{p:?}: {e}"),
    }
}

fn remover_arquivo(p: &Path) -> ResultType<()> {
    match fs::remove_file(p) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(e) => bail!("apagar {p:?}: {e}"),
    }
}

/// Abre a pasta `troca` para uma atualização nova (R4, achados 1 e 2). Ordem:
/// 1. confere a instalação (`atual`, 2 níveis estritos) e a parte EXISTENTE da cadeia de
///    `troca` (`troca` se existe; senão `<App>`; senão `ProgramData`), ancestrais incluídos,
///    antes de ler qualquer marcador ou apagar qualquer coisa;
/// 2. cria o que falta (`<App>`, `troca`) já protegido e reconfere `troca`;
/// 3. pega a trava; ocupada = outra atualização ou a recuperação rodando: sai;
/// 4. com a trava, pela fase: `TrocaIniciada` = recusa sem tocar em nada (tarefas mantidas);
///    as demais = descarta os restos (tarefas, scripts e marcadores; `.novo`; `.anterior` e
///    `.falhou` só com a atual tendo o EXE), cada irmã conferida antes de apagar.
/// Devolve a trava, que o chamador segura até o fim da troca.
pub(crate) fn abrir_troca<T>(c: &Caminhos, exe: &str, a: &mut Abrir<T>) -> ResultType<T> {
    let base = c
        .troca
        .parent()
        .ok_or_else(|| anyhow!("sem pasta-mãe: {:?}", c.troca))?;
    let raiz_dados = base
        .parent()
        .ok_or_else(|| anyhow!("sem pasta-mãe: {:?}", base))?;
    cadeia(a.ler, &c.atual, 2)?;
    if existe(&c.troca)? {
        cadeia(a.ler, &c.troca, 1)?;
    } else if existe(base)? {
        cadeia(a.ler, base, 0)?;
        (a.criar)(&c.troca).map_err(|e| anyhow!("criar {:?}: {e}", c.troca))?;
    } else {
        cadeia(a.ler, raiz_dados, 0)?;
        (a.criar)(base).map_err(|e| anyhow!("criar {:?}: {e}", base))?;
        (a.criar)(&c.troca).map_err(|e| anyhow!("criar {:?}: {e}", c.troca))?;
    }
    cadeia(a.ler, &c.troca, 1)?;
    let trava = (a.travar)(&c.troca.join(TRAVA))
        .map_err(|e| anyhow!("trava da recuperação ocupada ou inacessível: {e}"))?;
    let fase = c.fase();
    log::info!("[portal-update] fase da atualização anterior: {fase:?}");
    if fase == Fase::TrocaIniciada {
        bail!(
            "troca anterior iniciada e não concluída ({:?} sem {}); a recuperação continua \
             com as tarefas; não atualizo",
            c.troca,
            MARCADOR_CONCLUIDO
        );
    }
    (a.apagar_tarefas)();
    // `iniciada` antes de `concluido`: queda no meio nunca vira "iniciada sem concluido".
    for nome in [MARCADOR_INICIADA, MARCADOR_CONCLUIDO, SCRIPT_RECUPERAR, SCRIPT_GUARDA] {
        remover_arquivo(&c.troca.join(nome))?;
    }
    let atual_ok = c.atual.join(exe).is_file();
    let afastadas: Vec<PathBuf> = (1..=AFASTADAS).map(|n| c.afastada(n)).collect();
    for d in [&c.novo, &c.anterior, &c.falhou].into_iter().chain(&afastadas) {
        if !existe(d)? {
            continue;
        }
        if *d != c.novo && !atual_ok {
            bail!("instalação atual sem {exe}; não apago {d:?}");
        }
        cadeia(a.ler, d, 2)?;
        remover_arvore(d)?;
    }
    Ok(trava)
}

/// Grava um marcador novo (falha se existir) e confere o que ficou no disco.
pub(crate) fn gravar_marcador(p: &Path) -> ResultType<()> {
    use std::io::Write;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(p)
        .map_err(|e| anyhow!("gravar {p:?}: {e}"))?;
    f.write_all(b"ok")?;
    f.sync_all()?;
    drop(f);
    if fs::read(p)? != b"ok" {
        bail!("{p:?} não ficou gravado");
    }
    Ok(())
}

fn remover_arvore(d: &Path) -> ResultType<()> {
    match fs::symlink_metadata(d) {
        Ok(m) if m.is_dir() && !m.file_type().is_symlink() => fs::remove_dir_all(d)?,
        Ok(_) => fs::remove_file(d)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

pub(crate) fn sha256_arquivo(p: &Path) -> io::Result<[u8; 32]> {
    let mut f = fs::File::open(p)?;
    let mut h = Sha256::new();
    io::copy(&mut f, &mut h)?;
    Ok(h.finalize().into())
}

/// Copiador padrão. Os testes injetam falhas por aqui.
pub(crate) fn copiar_arquivo(de: &Path, para: &Path) -> io::Result<()> {
    fs::copy(de, para).map(|_| ())
}

pub(crate) struct Preparo<'a> {
    /// Pasta da versão nova (a do EXE que roda `--update`).
    pub origem: &'a Path,
    /// Nome do EXE dentro de `origem` (ex.: `rustdesk.exe`).
    pub exe_origem: &'a str,
    /// Nome do EXE instalado (ex.: `SuporteVancin.exe`).
    pub exe_destino: &'a str,
    /// Arquivos de fora da origem: (caminho, nome dentro da instalação).
    pub extras: &'a [(PathBuf, String)],
    /// Arquivos a não levar para a nova instalação (ex.: `meta.toml`).
    pub remover: &'a [&'a str],
    /// Cria `.novo` (deve falhar se já existir). No Windows: `acl::criar_pasta_protegida`.
    pub criar_novo: fn(&Path) -> io::Result<()>,
}

/// `criar_novo` sem ACL (testes e sistemas não Windows).
pub(crate) fn criar_pasta(p: &Path) -> io::Result<()> {
    fs::create_dir(p)
}

/// Lista arquivos regulares (relativos) de `raiz`, sem seguir links.
fn listar(raiz: &Path) -> io::Result<Vec<PathBuf>> {
    let mut saida = Vec::new();
    let mut pilha = vec![PathBuf::new()];
    while let Some(rel) = pilha.pop() {
        for e in fs::read_dir(raiz.join(&rel))? {
            let e = e?;
            let t = e.file_type()?;
            let r = rel.join(e.file_name());
            if t.is_symlink() {
                continue;
            } else if t.is_dir() {
                pilha.push(r);
            } else if t.is_file() {
                saida.push(r);
            }
        }
    }
    saida.sort();
    Ok(saida)
}

/// Monta `c.novo` (ver módulo). Devolve o número de arquivos conferidos.
pub(crate) fn preparar(
    c: &Caminhos,
    p: &Preparo,
    copiar: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> ResultType<usize> {
    remover_arvore(&c.novo)?;
    let r = preparar_dentro(c, p, copiar);
    if r.is_err() {
        if let Err(e) = remover_arvore(&c.novo) {
            log::warn!("[portal-update] não consegui apagar {:?}: {}", c.novo, e);
        }
    }
    r
}

fn preparar_dentro(
    c: &Caminhos,
    p: &Preparo,
    copiar: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
) -> ResultType<usize> {
    if !c.atual.join(p.exe_destino).is_file() {
        bail!("instalação atual sem {}", p.exe_destino);
    }
    if !p.origem.join(p.exe_origem).is_file() {
        bail!("versão nova sem {}", p.exe_origem);
    }
    // Já com a DACL final (subpastas e arquivos herdam); falha se alguém a recriou no meio.
    (p.criar_novo)(&c.novo).map_err(|e| anyhow!("criar {:?}: {}", c.novo, e))?;
    // destino relativo -> arquivo de origem (o último vence: atual, depois nova, depois extras)
    let mut plano: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for r in listar(&c.atual)? {
        plano.insert(r.clone(), c.atual.join(&r));
    }
    for r in listar(p.origem)? {
        let destino = if r == Path::new(p.exe_origem) {
            PathBuf::from(p.exe_destino)
        } else {
            r.clone()
        };
        plano.insert(destino, p.origem.join(&r));
    }
    for (de, nome) in p.extras {
        if de.is_file() {
            plano.insert(PathBuf::from(nome), de.clone());
        }
    }
    for nome in p.remover {
        plano.remove(Path::new(nome));
    }
    for (rel, de) in &plano {
        let para = c.novo.join(rel);
        if let Some(pai) = para.parent() {
            fs::create_dir_all(pai)?;
        }
        copiar(de, &para).map_err(|e| anyhow!("cópia de {:?} falhou: {}", rel, e))?;
    }
    // Confere tudo depois de copiar: pega cópia parcial/truncada que não deu erro.
    for (rel, de) in &plano {
        let a = sha256_arquivo(de)?;
        let b = sha256_arquivo(&c.novo.join(rel))
            .map_err(|e| anyhow!("{:?} ausente na nova instalação: {}", rel, e))?;
        if a != b {
            bail!("{:?} diferente da origem na nova instalação (cópia parcial?)", rel);
        }
    }
    if !c.novo.join(p.exe_destino).is_file() {
        bail!("nova instalação sem {}", p.exe_destino);
    }
    Ok(plano.len())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Servico {
    Rodando,
    Parado,
    /// Subindo/parando/pausado.
    Transicao,
    /// Não deu para consultar.
    Desconhecido,
}

/// O que a troca precisa do sistema. No Windows: SCM, taskkill e `MoveFileEx`.
pub(crate) trait Sistema {
    fn servico(&mut self) -> Servico;
    fn parar_servico(&mut self);
    fn iniciar_servico(&mut self);
    /// Encerra os processos do app (menos este) que podem travar a pasta.
    fn encerrar_processos(&mut self);
    /// Falha se `para` existir (como o `ren` do cmd).
    fn renomear(&mut self, de: &Path, para: &Path) -> io::Result<()>;
    fn esperar(&mut self, d: Duration);
    /// Apaga as tarefas agendadas da recuperação (as duas fixas e a `religar`).
    fn apagar_tarefas(&mut self);
    /// `rd /s /q`: pode falhar e deixar a pasta (arquivo preso por outro processo).
    fn apagar(&mut self, p: &Path) -> io::Result<()> {
        remover_arvore(p).map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }

    // Só os modos do `recuperar.bat` (fora da trava) usam os métodos abaixo. No Windows quem
    // os executa é o próprio BAT (`schtasks`, descritor 9); `SistemaWindows` não interpreta
    // programas, então o padrão recusa (o programa termina `Pendente` sem mexer em nada).

    /// Pega a trava `recuperar.lock` (a mesma da troca e de toda execução do BAT); `false` =
    /// ocupada.
    fn travar(&mut self) -> bool {
        false
    }
    fn soltar(&mut self) {}
    /// Registra a tarefa `religar` (ao iniciar: reabilita as fixas e roda o automático).
    fn criar_religar(&mut self) -> bool {
        false
    }
    /// Desabilita as tarefas fixas que existem; `false` = alguma existe e não desabilitou.
    fn desabilitar_tarefas(&mut self) -> bool {
        false
    }
    /// `schtasks /Change /ENABLE` na fixa `i` (0 = ao iniciar, 1 = de minuto). O resultado do
    /// comando não decide nada: quem decide é `tarefa_habilitada`, consultada em seguida.
    fn habilitar_tarefa(&mut self, _i: usize) {}
    /// Consulta da fixa `i`: `Some(true)` = existe e está habilitada; `Some(false)` = existe e
    /// está desabilitada (ou a consulta falhou); `None` = não existe. O padrão (`Some(false)`)
    /// nunca confirma, de modo que um `Sistema` sem agenda leva a `SemRecuperacao`.
    fn tarefa_habilitada(&mut self, _i: usize) -> Option<bool> {
        Some(false)
    }
}

/// As tarefas fixas da recuperação: ao iniciar (índice 0) e a cada minuto (índice 1).
pub(crate) const TAREFAS_FIXAS: usize = 2;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tempos {
    pub passo: Duration,
    /// Consultas (uma por `passo`) esperando o serviço parar.
    pub parar: u32,
    /// Consultas esperando o serviço chegar a RUNNING.
    pub subir: u32,
    /// Consultas seguidas em RUNNING para considerar que subiu de verdade.
    pub estavel: u32,
    /// Tentativas de renomear uma pasta (antivírus/indexador seguram arquivos por instantes).
    pub renomear: u32,
    /// Modo `manual`: novas tentativas (uma a cada 3 `passo`s) de pegar a trava enquanto
    /// outra execução (guarda, tarefa, troca) a segura.
    pub trava: u32,
    /// Tentativas de reabilitar e confirmar cada tarefa fixa (2 `passo`s entre elas).
    pub habilitar: u32,
}

impl Tempos {
    /// Pior caso ≈ 60 + 3×(20×3) + 60 + 20 + (60 + 60) s ≈ 8 min; a guarda espera até 20 min.
    pub const PADRAO: Tempos = Tempos {
        passo: Duration::from_secs(1),
        parar: 60,
        subir: 60,
        estavel: 20,
        renomear: 20,
        // 400 × 3 s = 20 min: mais que o pior caso de uma execução (≈ 8 min).
        trava: 400,
        // 5 × (≈ 2 s + consulta): o serviço de agenda ocupado por instantes não vira falha.
        habilitar: 5,
    };
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resultado {
    /// Nova instalação no lugar e serviço estável.
    Atualizado,
    /// Nada foi trocado; a instalação anterior continua e o serviço foi religado.
    NaoTrocado(String),
    /// A nova foi colocada mas não subiu; a anterior voltou e o serviço dela subiu.
    Revertido(String),
    /// Não foi possível garantir o estado: fica para a guarda/tarefa (`recuperar.bat`).
    Critico(String),
}

impl Resultado {
    /// Estado consistente (a guarda não precisa agir).
    pub fn consistente(&self) -> bool {
        !matches!(self, Resultado::Critico(_))
    }
}

fn esperar_estado(s: &mut dyn Sistema, alvo: Servico, consultas: u32, t: &Tempos) -> bool {
    for i in 0..=consultas {
        if s.servico() == alvo {
            return true;
        }
        if i < consultas {
            s.esperar(t.passo);
        }
    }
    false
}

fn renomear_tentando(
    s: &mut dyn Sistema,
    de: &Path,
    para: &Path,
    tentativas: u32,
    t: &Tempos,
) -> io::Result<()> {
    let mut ultimo = None;
    for i in 0..tentativas.max(1) {
        match s.renomear(de, para) {
            Ok(()) => return Ok(()),
            Err(e) => {
                log::warn!(
                    "[portal-update] renomear {:?} -> {:?} (tentativa {}): {}",
                    de,
                    para,
                    i + 1,
                    e
                );
                ultimo = Some(e);
                s.encerrar_processos();
                s.esperar(t.passo * 3);
            }
        }
    }
    Err(ultimo.unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, "renomear")))
}

/// Sobe o serviço e exige RUNNING por `estavel` consultas seguidas.
fn subir_estavel(s: &mut dyn Sistema, t: &Tempos) -> bool {
    s.iniciar_servico();
    if !esperar_estado(s, Servico::Rodando, t.subir, t) {
        return false;
    }
    for _ in 0..t.estavel {
        s.esperar(t.passo);
        if s.servico() != Servico::Rodando {
            return false;
        }
    }
    true
}

/// Religa (se o serviço estava rodando antes) e exige RUNNING estável. Partida não
/// confirmada = `Critico`: fica para a guarda/tarefas e ninguém grava `concluido`.
fn religado(s: &mut dyn Sistema, religar: bool, t: &Tempos, r: Resultado) -> Resultado {
    if !religar || subir_estavel(s, t) {
        return r;
    }
    let antes = match r {
        Resultado::Atualizado => "atualizado".to_owned(),
        Resultado::NaoTrocado(m) | Resultado::Revertido(m) | Resultado::Critico(m) => m,
    };
    Resultado::Critico(format!("{antes}; serviço não confirmou RUNNING"))
}

/// Troca `c.atual` por `c.novo` (já preparado e conferido). `religar`: o serviço estava
/// rodando antes e precisa estar rodando depois. Toda partida é confirmada (`subir_estavel`);
/// sem confirmação o resultado é `Critico`.
pub(crate) fn trocar(c: &Caminhos, s: &mut dyn Sistema, religar: bool, t: &Tempos) -> Resultado {
    ponto_bancada("antes_parar");
    s.parar_servico();
    if !esperar_estado(s, Servico::Parado, t.parar, t) {
        log::warn!("[portal-update] serviço não parou a tempo; encerrando processos");
    }
    s.encerrar_processos();

    ponto_bancada("antes_ren_atual");
    if let Err(e) = renomear_tentando(s, &c.atual, &c.anterior, t.renomear, t) {
        // Nada mudou no disco.
        let r = Resultado::NaoTrocado(format!("instalação atual presa: {e}"));
        return religado(s, religar, t, r);
    }
    ponto_bancada("entre_ren");
    if let Err(e) = renomear_tentando(s, &c.novo, &c.atual, t.renomear, t) {
        return match renomear_tentando(s, &c.anterior, &c.atual, t.renomear * 3, t) {
            Ok(()) => {
                let r = Resultado::NaoTrocado(format!("nova instalação não entrou no lugar: {e}"));
                religado(s, religar, t, r)
            }
            Err(e2) => Resultado::Critico(format!(
                "nova não entrou ({e}) e a anterior não voltou ({e2})"
            )),
        };
    }
    ponto_bancada("antes_subir");
    if !religar || subir_estavel(s, t) {
        return Resultado::Atualizado;
    }

    // A nova não subiu: volta a anterior.
    log::error!("[portal-update] serviço da versão nova não ficou RUNNING; voltando a anterior");
    s.parar_servico();
    esperar_estado(s, Servico::Parado, t.parar, t);
    s.encerrar_processos();
    if let Err(e) = remover_arvore(&c.falhou) {
        log::warn!("[portal-update] não consegui apagar {:?}: {}", c.falhou, e);
    }
    if let Err(e) = renomear_tentando(s, &c.atual, &c.falhou, t.renomear * 3, t) {
        // A nova continua no lugar; ao menos tenta subir.
        subir_estavel(s, t);
        return Resultado::Critico(format!("versão nova não subiu e não saiu do lugar: {e}"));
    }
    if let Err(e) = renomear_tentando(s, &c.anterior, &c.atual, t.renomear * 3, t) {
        // Não deixa a pasta vazia: devolve a nova e tenta subir.
        let _ = renomear_tentando(s, &c.falhou, &c.atual, t.renomear * 3, t);
        subir_estavel(s, t);
        return Resultado::Critico(format!("anterior não voltou: {e}"));
    }
    let r = Resultado::Revertido("serviço da versão nova não ficou RUNNING".to_owned());
    religado(s, true, t, r)
}

// ---------------------------------------------------------------------------------------
// Pontos de parada da bancada (R6, item 2). Só existem no build com a feature de compilação
// `bancada-pausas` (desligada no build normal e no de release): sem ela, `ponto_bancada` é
// vazia e o módulo `bancada` (com o nome da variável de ambiente) nem é compilado.
// ---------------------------------------------------------------------------------------

/// `true` só no build de bancada. O teste `build_normal_nao_tem_pausas` usa isto.
pub(crate) const PAUSAS_DE_BANCADA: bool = cfg!(feature = "bancada-pausas");

/// Fases em que a bancada pode parar o atualizador, na ordem em que ocorrem.
pub(crate) const FASES_BANCADA: [&str; 10] = [
    "antes_preparar",
    "antes_scripts",
    "antes_tarefas",
    "antes_guarda",
    "antes_iniciada",
    "antes_parar",
    "antes_ren_atual",
    "entre_ren",
    "antes_subir",
    "antes_concluido",
];

/// Build de bancada: para na `fase` se pedida (ver `bancada::ponto_com`). Build normal: nada.
#[cfg(feature = "bancada-pausas")]
pub(crate) fn ponto_bancada(fase: &str) {
    bancada::ponto(fase)
}

/// Build normal: vazia (nem lê variável de ambiente).
#[cfg(not(feature = "bancada-pausas"))]
#[inline(always)]
pub(crate) fn ponto_bancada(fase: &str) {
    debug_assert!(FASES_BANCADA.contains(&fase), "fase de bancada desconhecida: {fase}");
}

#[cfg(any(feature = "bancada-pausas", test))]
pub(crate) mod bancada {
    use hbb_common::log;
    use std::{path::Path, time::Duration};

    /// Lista de fases separadas por vírgula, ou `todas`.
    pub(crate) const VAR_PAUSA: &str = "SUPORTE_BANCADA_PAUSA";
    /// Pasta dos arquivos de controle (padrão `C:\bancada\pausa`).
    pub(crate) const VAR_DIR: &str = "SUPORTE_BANCADA_DIR";

    #[cfg(feature = "bancada-pausas")]
    pub(crate) fn ponto(fase: &str) {
        let lista = std::env::var(VAR_PAUSA).ok();
        let dir = std::env::var_os(VAR_DIR).unwrap_or_else(|| r"C:\bancada\pausa".into());
        ponto_com(fase, lista.as_deref(), Path::new(&dir), Duration::from_millis(200), None);
    }

    pub(crate) fn pedida(fase: &str, lista: &str) -> bool {
        lista
            .split(',')
            .map(str::trim)
            .any(|f| f.eq_ignore_ascii_case("todas") || f == fase)
    }

    /// Fase pedida: grava `<dir>\parado-<fase>.txt` (com o PID deste processo, para o
    /// `taskkill /PID`) e espera até existir `<dir>\liberar-<fase>`, que apaga junto com o
    /// `parado-`. `limite` = máximo de consultas (testes); `None` = sem limite (a guarda encerra
    /// o processo depois de ≈ 20 min). Devolve se parou.
    pub(crate) fn ponto_com(
        fase: &str,
        lista: Option<&str>,
        dir: &Path,
        espera: Duration,
        limite: Option<u32>,
    ) -> bool {
        let Some(lista) = lista else { return false };
        if !pedida(fase, lista) {
            return false;
        }
        let parado = dir.join(format!("parado-{fase}.txt"));
        let liberar = dir.join(format!("liberar-{fase}"));
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&parado, format!("pid={}\r\nfase={fase}\r\n", std::process::id()));
        log::warn!("[portal-update] BANCADA: parado em {fase}; crie {liberar:?} para seguir");
        let mut n = 0u32;
        while !liberar.exists() {
            if limite.map_or(false, |l| n >= l) {
                return true;
            }
            n += 1;
            std::thread::sleep(espera);
        }
        let _ = std::fs::remove_file(&liberar);
        let _ = std::fs::remove_file(&parado);
        log::warn!("[portal-update] BANCADA: liberado em {fase}");
        true
    }
}

/// Pontos em que a proteção das pastas é conferida de novo (achado 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Momento {
    /// `.novo` montado; antes de executar o EXE dele.
    AntesDeExecutar,
    /// Guarda e tarefas prontas; antes de parar o serviço e trocar.
    AntesDeTrocar,
}

/// Fluxo completo (depois de `abrir_troca`): prepara e confere `.novo`, prepara a guarda
/// (`antes_de_parar`), grava `iniciada` e só então troca. Falha antes da troca não toca o
/// serviço nem a instalação atual. `conferir` refaz a conferência de ACL/reparse/hash nos
/// `Momento`s; erro = não troca.
pub(crate) fn atualizar(
    c: &Caminhos,
    p: &Preparo,
    copiar: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    conferir: &mut dyn FnMut(Momento) -> ResultType<()>,
    validar: &mut dyn FnMut(&Path) -> ResultType<()>,
    antes_de_parar: &mut dyn FnMut() -> ResultType<()>,
    s: &mut dyn Sistema,
    religar: bool,
    t: &Tempos,
) -> Resultado {
    let antes = preparar(c, p, copiar).and_then(|n| {
        log::info!("[portal-update] nova instalação montada e conferida ({n} arquivos)");
        conferir(Momento::AntesDeExecutar)?;
        validar(&c.novo.join(p.exe_destino))?;
        antes_de_parar()?;
        conferir(Momento::AntesDeTrocar)?;
        // Recuperação registrada e conferida: daqui em diante a troca é dela (achado 2).
        ponto_bancada("antes_iniciada");
        gravar_marcador(&c.iniciada())
    });
    if let Err(e) = antes {
        if let Err(e2) = remover_arvore(&c.novo) {
            log::warn!("[portal-update] não consegui apagar {:?}: {}", c.novo, e2);
        }
        return Resultado::NaoTrocado(format!("antes de parar o serviço: {e:#}"));
    }
    trocar(c, s, religar, t)
}

// ---------------------------------------------------------------------------------------
// Recuperação: programa único, interpretado nos testes e convertido em `recuperar.bat`.
// ---------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Pasta {
    Atual,
    Anterior,
    Falhou,
}

impl Pasta {
    pub const TODAS: [Pasta; 3] = [Pasta::Atual, Pasta::Anterior, Pasta::Falhou];
    fn var(self) -> &'static str {
        match self {
            Pasta::Atual => "ATUAL",
            Pasta::Anterior => "ANTERIOR",
            Pasta::Falhou => "FALHOU",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Cond {
    Concluido,
    Existe(Pasta),
    /// A pasta tem o EXE do app (instalação utilizável).
    ExisteExe(Pasta),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Saida {
    /// Serviço RUNNING estável, `concluido` gravado e conferido, tarefas apagadas.
    Concluida,
    /// Não deu para garantir: tarefas mantidas, sem `concluido`; repete no próximo disparo.
    Pendente,
    /// Outra execução segura a trava: nada foi tocado (código 3).
    Ocupada,
    /// Só nos modos (R6, item 1): a recuperação não concluiu E alguma tarefa fixa não foi
    /// confirmada habilitada depois das novas tentativas. A recuperação automática NÃO está
    /// restabelecida; o BAT mostra o comando para religar à mão (código 4).
    SemRecuperacao,
}

impl Saida {
    pub(crate) fn codigo(self) -> u8 {
        match self {
            Saida::Concluida => 0,
            Saida::Pendente => 1,
            Saida::Ocupada => 3,
            Saida::SemRecuperacao => 4,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Passo {
    Rotulo(&'static str),
    /// `if [not] <cond> goto ir`
    Se { cond: Cond, nao: bool, ir: &'static str },
    Ir(&'static str),
    Log(&'static str),
    /// `Log` e também na tela (modo manual, rodado pelo técnico).
    Aviso(&'static str),
    /// Para o serviço, espera STOPPED (`Tempos::parar`) e encerra os processos do app.
    PararServico,
    /// `rd /s /q` (erro ignorado: o passo seguinte confere se a pasta ficou).
    Apagar(Pasta),
    /// `ren` com novas tentativas; falha se o destino existir; confere o destino depois.
    Renomear { de: Pasta, para: Pasta, falha: &'static str },
    /// `.falhou` que não saiu com `Apagar` vai para o primeiro `.falhou.<n>` livre
    /// (`AFASTADAS`); confere que o destino existe e `.falhou` não.
    Afastar { falha: &'static str },
    /// Sobe o serviço e exige RUNNING estável (`subir_estavel`).
    SubirEstavel { falha: &'static str },
    /// Grava `concluido` e confere que ficou.
    MarcarConcluido { falha: &'static str },
    /// Apaga as duas tarefas fixas e a `religar`.
    ApagarTarefas,
    // ---- só fora da trava (modos) ----
    /// Registra a tarefa `religar` (ao iniciar, SYSTEM: `recuperar.bat religar`).
    CriarReligar { falha: &'static str },
    /// `schtasks /Change /DISABLE` em cada tarefa fixa que existe (sumiu = ok).
    DesabilitarTarefas { falha: &'static str },
    /// Para cada tarefa fixa: `schtasks /Change /ENABLE` e consulta (PowerShell,
    /// `Settings.Enabled`), repetindo `Tempos::habilitar` vezes. Passa adiante se TODAS foram
    /// confirmadas habilitadas (ou não existem e `concluido` existe: uma execução concluiu e as
    /// apagou); senão, depois de tentar todas, vai para `falha` (R6, item 1).
    HabilitarTarefas { falha: &'static str },
    /// Mostra e registra que a recuperação automática NÃO foi restabelecida, com o comando para
    /// religar cada tarefa fixa à mão.
    ComoReabilitar,
    /// Pega a trava e roda o corpo (`programa_recuperar`) com ela; solta ao sair dele.
    /// Ocupada: `esperar` = tenta de novo `Tempos::trava` vezes (3 s cada); senão desiste.
    ComTrava {
        esperar: bool,
        concluida: &'static str,
        pendente: &'static str,
        ocupada: &'static str,
    },
    Sair(Saida),
}

impl Passo {
    /// Passos que só podem rodar com a trava (corpo).
    fn do_corpo(&self) -> bool {
        matches!(
            self,
            Passo::Se { .. }
                | Passo::PararServico
                | Passo::Apagar(_)
                | Passo::Renomear { .. }
                | Passo::Afastar { .. }
                | Passo::SubirEstavel { .. }
                | Passo::MarcarConcluido { .. }
                | Passo::ApagarTarefas
        )
    }
    /// Passos que só podem rodar fora da trava (modos).
    fn do_modo(&self) -> bool {
        matches!(
            self,
            Passo::CriarReligar { .. }
                | Passo::DesabilitarTarefas { .. }
                | Passo::HabilitarTarefas { .. }
                | Passo::ComoReabilitar
                | Passo::ComTrava { .. }
        )
    }
    fn destinos(&self) -> Vec<&'static str> {
        match *self {
            Passo::Se { ir, .. } | Passo::Ir(ir) => vec![ir],
            Passo::Renomear { falha, .. }
            | Passo::Afastar { falha }
            | Passo::SubirEstavel { falha }
            | Passo::MarcarConcluido { falha }
            | Passo::CriarReligar { falha }
            | Passo::DesabilitarTarefas { falha }
            | Passo::HabilitarTarefas { falha } => vec![falha],
            Passo::ComTrava { concluida, pendente, ocupada, .. } => vec![concluida, pendente, ocupada],
            _ => vec![],
        }
    }
}

/// Corpo da recuperação (roda sempre com a trava). Invariante: nenhum passo deixa as três
/// pastas sem o EXE (só renomeia uma pasta para um nome livre; só apaga `.falhou` quando
/// `.anterior` tem o EXE; um `.falhou` que não sai é afastado, nunca sobrescrito).
pub(crate) fn programa_recuperar() -> Vec<Passo> {
    use Cond::*;
    use Pasta::*;
    use Passo::*;
    vec![
        Log("inicio"),
        // `concluido` só dispensa a tabela se a atual estiver utilizável.
        Se { cond: Concluido, nao: true, ir: "tabela" },
        Se { cond: ExisteExe(Atual), nao: false, ir: "garantir" },
        Rotulo("tabela"),
        Se { cond: ExisteExe(Anterior), nao: true, ir: "sem_anterior" },
        // Troca interrompida: volta a anterior.
        PararServico,
        Se { cond: Existe(Atual), nao: true, ir: "restaurar" },
        Apagar(Falhou),
        // R5: `.falhou` que não saiu (arquivo preso) é afastado em vez de travar a tabela.
        Se { cond: Existe(Falhou), nao: true, ir: "mover_atual" },
        Log("falhou presa afastando"),
        Afastar { falha: "garantir" },
        Rotulo("mover_atual"),
        Renomear { de: Atual, para: Falhou, falha: "garantir" },
        Rotulo("restaurar"),
        Renomear { de: Anterior, para: Atual, falha: "devolver" },
        Log("anterior restaurada"),
        Ir("garantir"),
        // A anterior não voltou: nunca deixar sem instalação.
        Rotulo("devolver"),
        Se { cond: Existe(Atual), nao: false, ir: "garantir" },
        Se { cond: ExisteExe(Falhou), nao: true, ir: "pendente" },
        Renomear { de: Falhou, para: Atual, falha: "pendente" },
        Ir("garantir"),
        // Sem anterior: a atual é a única; se ela sumiu, a que falhou volta ao lugar.
        Rotulo("sem_anterior"),
        Se { cond: Existe(Atual), nao: false, ir: "garantir" },
        Se { cond: ExisteExe(Falhou), nao: true, ir: "pendente" },
        Renomear { de: Falhou, para: Atual, falha: "pendente" },
        Rotulo("garantir"),
        SubirEstavel { falha: "pendente" },
        MarcarConcluido { falha: "pendente" },
        ApagarTarefas,
        Log("fim"),
        Sair(Saida::Concluida),
        Rotulo("pendente"),
        Log("pendente tarefas mantidas"),
        Sair(Saida::Pendente),
    ]
}

/// Sem argumento (guarda, tarefa ao iniciar, tarefa de minuto): trava livre = corpo; ocupada =
/// sai com 3 sem tocar em nada.
pub(crate) fn programa_automatico() -> Vec<Passo> {
    use Passo::*;
    vec![
        ComTrava {
            esperar: false,
            concluida: "auto_ok",
            pendente: "auto_pendente",
            ocupada: "auto_ocupado",
        },
        Rotulo("auto_ok"),
        Sair(Saida::Concluida),
        Rotulo("auto_pendente"),
        Sair(Saida::Pendente),
        Rotulo("auto_ocupado"),
        Log("ocupado"),
        Sair(Saida::Ocupada),
    ]
}

/// `recuperar.bat manual` (R5, achado 1), rodado pelo técnico como administrador:
/// 1. registra `religar` (se cair daqui em diante, o boot reabilita as tarefas);
/// 2. desabilita as duas tarefas fixas (a execução em curso continua até sair);
/// 3. espera a trava — a mesma da troca, da guarda e das tarefas: enquanto qualquer uma delas
///    roda o corpo, este modo não entra; depois que entra, nenhuma delas entra (saem com 3);
/// 4. roda o mesmo corpo; as tarefas (e `religar`) só são apagadas por ele, depois de RUNNING
///    estável e `concluido`; a trava é solta ao sair do corpo;
/// 5. corpo pendente, trava que não veio ou falha ao desabilitar: reabilita as tarefas e
///    CONFIRMA por consulta que as duas fixas ficaram habilitadas (com novas tentativas); só
///    então diz "reabilitadas" e sai com 1 (a recuperação automática retoma no próximo minuto);
/// 6. sem `religar` registrada: nada foi desabilitado, mas confere do mesmo jeito que as fixas
///    estão habilitadas (uma execução anterior que caiu pode tê-las deixado desabilitadas);
/// 7. alguma fixa não confirmada (R6, item 1): diz que a recuperação automática NÃO foi
///    restabelecida, mostra o comando para religar à mão e sai com 4 (`SemRecuperacao`). A
///    `religar` continua registrada: no próximo boot ela tenta de novo.
pub(crate) fn programa_manual() -> Vec<Passo> {
    use Passo::*;
    vec![
        Aviso("manual inicio"),
        CriarReligar { falha: "manual_sem_religar" },
        DesabilitarTarefas { falha: "manual_reabilitar" },
        Aviso("tarefas desabilitadas aguardando a trava"),
        ComTrava {
            esperar: true,
            concluida: "manual_ok",
            pendente: "manual_reabilitar",
            ocupada: "manual_reabilitar",
        },
        Rotulo("manual_ok"),
        Aviso("manual concluida servico rodando tarefas apagadas"),
        Sair(Saida::Concluida),
        Rotulo("manual_reabilitar"),
        HabilitarTarefas { falha: "manual_sem_recuperacao" },
        Aviso("manual pendente tarefas reabilitadas e conferidas"),
        Sair(Saida::Pendente),
        Rotulo("manual_sem_religar"),
        HabilitarTarefas { falha: "manual_sem_recuperacao" },
        Aviso("manual sem a tarefa religar nada foi mudado tarefas conferidas"),
        Sair(Saida::Pendente),
        Rotulo("manual_sem_recuperacao"),
        ComoReabilitar,
        Sair(Saida::SemRecuperacao),
    ]
}

/// `recuperar.bat religar` (tarefa ao iniciar criada pelo modo manual): reabilita e confere as
/// duas tarefas fixas — inclusive a de minuto — e roda o automático. Fica registrada até o
/// corpo concluir (que a apaga). Fixa não confirmada: registra o comando para religar à mão e
/// roda o corpo assim mesmo (é a única chance deste boot); se ele não concluir, sai com 4.
pub(crate) fn programa_religar() -> Vec<Passo> {
    use Passo::*;
    vec![
        Log("religar"),
        HabilitarTarefas { falha: "religar_sem_tarefas" },
        ComTrava {
            esperar: false,
            concluida: "religar_ok",
            pendente: "religar_pendente",
            ocupada: "religar_ocupado",
        },
        Rotulo("religar_ok"),
        Sair(Saida::Concluida),
        Rotulo("religar_pendente"),
        Sair(Saida::Pendente),
        Rotulo("religar_ocupado"),
        Sair(Saida::Ocupada),
        Rotulo("religar_sem_tarefas"),
        ComoReabilitar,
        ComTrava {
            esperar: false,
            concluida: "religar_ok",
            pendente: "religar_sem_recuperacao",
            ocupada: "religar_sem_recuperacao",
        },
        Rotulo("religar_sem_recuperacao"),
        Sair(Saida::SemRecuperacao),
    ]
}

/// Os quatro programas do `recuperar.bat`.
pub(crate) struct Modos {
    pub automatico: Vec<Passo>,
    pub manual: Vec<Passo>,
    pub religar: Vec<Passo>,
    pub corpo: Vec<Passo>,
}

impl Modos {
    pub fn padrao() -> Self {
        Self {
            automatico: programa_automatico(),
            manual: programa_manual(),
            religar: programa_religar(),
            corpo: programa_recuperar(),
        }
    }
}

/// Rótulos que o gerador usa por conta própria.
const ROTULOS_RESERVADOS: [&str; 5] = ["principal", "ren", "ren_loop", "manual", "religar"];

fn nome_simples(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_lowercase() || c == '_' || c == ' ')
}

/// Rótulos únicos e simples, todo destino existe, o programa termina em `Sair`/`Ir`.
pub(crate) fn validar_programa(prog: &[Passo]) -> ResultType<HashMap<&'static str, usize>> {
    let mut rotulos = HashMap::new();
    for (i, p) in prog.iter().enumerate() {
        match p {
            Passo::Rotulo(r) => {
                if !nome_simples(r) || r.contains(' ') || r.starts_with('_') {
                    bail!("rótulo inválido: {r:?}");
                }
                if ROTULOS_RESERVADOS.contains(r) {
                    bail!("rótulo reservado: {r}");
                }
                if rotulos.insert(*r, i).is_some() {
                    bail!("rótulo repetido: {r}");
                }
            }
            Passo::Log(m) | Passo::Aviso(m) if !nome_simples(m) => bail!("log inválido: {m:?}"),
            _ => {}
        }
    }
    for p in prog {
        for a in p.destinos() {
            if !rotulos.contains_key(a) {
                bail!("destino inexistente: {a}");
            }
        }
    }
    if !matches!(prog.last(), Some(Passo::Sair(_) | Passo::Ir(_))) {
        bail!("programa não termina em Sair/Ir");
    }
    Ok(rotulos)
}

/// Cada programa válido; o corpo só com passos de corpo e sem `Sair(Ocupada)`; os modos sem
/// nenhum passo que mexa em pasta, serviço ou marcador (tudo isso só com a trava); rótulos
/// únicos no BAT inteiro.
pub(crate) fn validar_modos(m: &Modos) -> ResultType<()> {
    let mut todos = std::collections::HashSet::new();
    for (nome, prog, corpo) in [
        ("automatico", &m.automatico, false),
        ("manual", &m.manual, false),
        ("religar", &m.religar, false),
        ("corpo", &m.corpo, true),
    ] {
        validar_programa(prog).map_err(|e| anyhow!("{nome}: {e}"))?;
        for p in prog.iter() {
            if corpo
                && (p.do_modo()
                    || *p == Passo::Sair(Saida::Ocupada)
                    || *p == Passo::Sair(Saida::SemRecuperacao))
            {
                bail!("{nome}: {p:?} não pode rodar com a trava");
            }
            if !corpo && p.do_corpo() {
                bail!("{nome}: {p:?} só pode rodar com a trava");
            }
            if let Passo::Rotulo(r) = p {
                if !todos.insert(*r) {
                    bail!("rótulo repetido entre programas: {r}");
                }
            }
        }
    }
    Ok(())
}

/// Execução retomável: um passo por `avancar`, para os testes intercalarem vários atores
/// (técnico, guarda, tarefas) sobre o mesmo `Sistema`. `ComTrava` roda o corpo num `Exec`
/// interno, um passo por vez, segurando a trava (`Sistema::travar`) do primeiro ao último.
/// Mesma semântica de cada passo no BAT.
pub(crate) struct Exec<'p> {
    prog: &'p [Passo],
    corpo: &'p [Passo],
    rotulos: HashMap<&'static str, usize>,
    i: usize,
    contador: u32,
    tentativas: u32,
    dentro: Option<Box<Exec<'p>>>,
}

impl<'p> Exec<'p> {
    pub fn novo(prog: &'p [Passo], corpo: &'p [Passo]) -> Self {
        let rotulos = validar_programa(prog).expect("programa válido");
        Self {
            prog,
            corpo,
            rotulos,
            i: 0,
            contador: 0,
            tentativas: 0,
            dentro: None,
        }
    }

    /// Está dentro do corpo (segura a trava).
    pub fn no_corpo(&self) -> bool {
        self.dentro.is_some()
    }

    /// Executa um passo: devolve o índice e o passo executado (o do corpo, se dentro dele) e a
    /// saída, quando o programa termina.
    pub fn avancar(
        &mut self,
        c: &Caminhos,
        exe: &str,
        s: &mut dyn Sistema,
        t: &Tempos,
    ) -> (usize, Passo, Option<Saida>) {
        self.contador += 1;
        assert!(self.contador < 1_000_000, "programa em laço");
        let aqui = self.i;
        let Some(&passo) = self.prog.get(aqui) else {
            return (aqui, Passo::Sair(Saida::Pendente), Some(Saida::Pendente));
        };
        if let Passo::ComTrava {
            esperar,
            concluida,
            pendente,
            ocupada,
        } = passo
        {
            if let Some(d) = self.dentro.as_mut() {
                let (j, p, fim) = d.avancar(c, exe, s, t);
                if let Some(x) = fim {
                    self.dentro = None;
                    s.soltar();
                    let r = if x == Saida::Concluida { concluida } else { pendente };
                    self.i = self.rotulos[r];
                }
                return (j, p, None);
            }
            if s.travar() {
                self.tentativas = 0;
                self.dentro = Some(Box::new(Exec::novo(self.corpo, self.corpo)));
            } else if self.tentativas >= if esperar { t.trava } else { 0 } {
                self.tentativas = 0;
                self.i = self.rotulos[ocupada];
            } else {
                self.tentativas += 1;
                s.esperar(t.passo * 3);
            }
            return (aqui, passo, None);
        }
        self.i += 1;
        let mut salto = None;
        match passo {
            Passo::Rotulo(_) | Passo::ComTrava { .. } => {}
            Passo::Se { cond, nao, ir } => {
                let v = match cond {
                    Cond::Concluido => c.concluido().exists(),
                    Cond::Existe(p) => c.pasta(p).exists(),
                    Cond::ExisteExe(p) => c.pasta(p).join(exe).is_file(),
                };
                if v != nao {
                    salto = Some(ir);
                }
            }
            Passo::Ir(r) => salto = Some(r),
            Passo::Log(m) | Passo::Aviso(m) => log::info!("[portal-update] recuperar: {m}"),
            Passo::PararServico => {
                s.parar_servico();
                esperar_estado(s, Servico::Parado, t.parar, t);
                s.encerrar_processos();
            }
            Passo::Apagar(p) => {
                if let Err(e) = s.apagar(c.pasta(p)) {
                    log::warn!("[portal-update] recuperar: apagar {:?}: {}", c.pasta(p), e);
                }
            }
            Passo::Renomear { de, para, falha } => {
                let (de, para) = (c.pasta(de), c.pasta(para));
                let ok = !para.exists()
                    && renomear_tentando(s, de, para, t.renomear, t).is_ok()
                    && para.exists();
                if !ok {
                    salto = Some(falha);
                }
            }
            Passo::Afastar { falha } => {
                let livre = (1..=AFASTADAS).map(|n| c.afastada(n)).find(|d| !d.exists());
                let ok = livre.map_or(false, |d| {
                    renomear_tentando(s, &c.falhou, &d, t.renomear, t).is_ok()
                        && d.exists()
                        && !c.falhou.exists()
                });
                if !ok {
                    salto = Some(falha);
                }
            }
            Passo::SubirEstavel { falha } => {
                if !subir_estavel(s, t) {
                    salto = Some(falha);
                }
            }
            Passo::MarcarConcluido { falha } => {
                let ok = fs::write(c.concluido(), "ok").is_ok() && c.concluido().is_file();
                if !ok {
                    salto = Some(falha);
                }
            }
            Passo::ApagarTarefas => s.apagar_tarefas(),
            Passo::CriarReligar { falha } => {
                if !s.criar_religar() {
                    salto = Some(falha);
                }
            }
            Passo::DesabilitarTarefas { falha } => {
                if !s.desabilitar_tarefas() {
                    salto = Some(falha);
                }
            }
            Passo::HabilitarTarefas { falha } => {
                if !habilitar_e_conferir(c, s, t) {
                    salto = Some(falha);
                }
            }
            Passo::ComoReabilitar => log::error!(
                "[portal-update] recuperar: ATENÇÃO, a recuperação automática NÃO foi \
                 restabelecida; religar as tarefas à mão (schtasks /Change /ENABLE)"
            ),
            Passo::Sair(x) => return (aqui, passo, Some(x)),
        }
        if let Some(r) = salto {
            self.i = self.rotulos[r];
        }
        (aqui, passo, None)
    }
}

/// `HabilitarTarefas` (mesma sequência do BAT): para cada fixa, até `Tempos::habilitar`
/// tentativas de `/ENABLE` + consulta, 2 `passo`s entre elas; tenta TODAS mesmo que uma falhe.
/// Não confirmada só é aceita se a tarefa não existe e `concluido` existe.
fn habilitar_e_conferir(c: &Caminhos, s: &mut dyn Sistema, t: &Tempos) -> bool {
    let mut todas = true;
    for i in 0..TAREFAS_FIXAS {
        let n = t.habilitar.max(1);
        let mut ok = false;
        for k in 0..n {
            s.habilitar_tarefa(i);
            if s.tarefa_habilitada(i) == Some(true) {
                ok = true;
                break;
            }
            if k + 1 < n {
                s.esperar(t.passo * 2);
            }
        }
        if !ok && !(c.concluido().exists() && s.tarefa_habilitada(i).is_none()) {
            log::error!("[portal-update] recuperar: tarefa fixa {i} não confirmada habilitada");
            todas = false;
        }
    }
    todas
}

/// Roda `prog` até o fim (corpo: `programa_recuperar`). `apos` é chamado depois de cada passo
/// (os testes conferem a invariante e simulam queda de energia ali).
pub(crate) fn executar(
    prog: &[Passo],
    c: &Caminhos,
    exe: &str,
    s: &mut dyn Sistema,
    t: &Tempos,
    apos: &mut dyn FnMut(usize, &Passo),
) -> Saida {
    let corpo = programa_recuperar();
    let mut e = Exec::novo(prog, &corpo);
    loop {
        let (i, p, fim) = e.avancar(c, exe, s, t);
        apos(i, &p);
        if let Some(x) = fim {
            return x;
        }
    }
}

/// Atalho: roda `programa_recuperar`; `true` = `Saida::Concluida`.
pub(crate) fn recuperar(c: &Caminhos, exe: &str, s: &mut dyn Sistema, t: &Tempos) -> bool {
    executar(&programa_recuperar(), c, exe, s, t, &mut |_, _| {}) == Saida::Concluida
}

/// Caracteres que o cmd interpreta mesmo entre aspas (ou que quebram a linha).
fn seguro_para_cmd(s: &str) -> bool {
    !s.is_empty() && !s.chars().any(|ch| "%!^\"&<>|\r\n".contains(ch) || ch.is_control())
}

fn texto(p: &Path) -> ResultType<String> {
    let s = p.to_str().ok_or_else(|| anyhow!("caminho não UTF-8: {:?}", p))?;
    if !seguro_para_cmd(s) {
        bail!("caminho com caractere especial para o cmd: {:?}", p);
    }
    Ok(s.to_owned())
}

fn nome_de(p: &Path) -> ResultType<String> {
    texto(Path::new(
        p.file_name().ok_or_else(|| anyhow!("sem nome: {:?}", p))?,
    ))
}

/// Tarefas agendadas da recuperação: as duas fixas (ao iniciar e a cada minuto, criadas pela
/// troca) e a `religar` (criada só pelo modo manual).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Tarefas<'a> {
    pub fixas: &'a [&'a str],
    pub religar: &'a str,
}

/// Gera `recuperar.bat` com os quatro programas (`Modos::padrao`). Roda pela guarda e pelas
/// tarefas (SYSTEM, sem argumento), pelo técnico (`manual`) e pela tarefa `religar`. O corpo
/// fica em `:principal` e só é chamado com o descritor 9 redirecionado para `recuperar.lock`:
/// se outro processo segura a trava, o redirecionamento falha, `:principal` não roda e `SAIDA`
/// continua 3. `ren` (não `move`, que aninharia a pasta se o destino existisse).
pub(crate) fn script_recuperar(
    c: &Caminhos,
    app: &str,
    broker: &str,
    tarefas: &Tarefas,
    t: &Tempos,
) -> ResultType<String> {
    script_de(&Modos::padrao(), c, app, broker, tarefas, t)
}

struct Gerador<'a> {
    c: &'a Caminhos,
    app: &'a str,
    exe: String,
    tarefas: &'a Tarefas<'a>,
    t: &'a Tempos,
    parar_processos: [String; 2],
}

impl Gerador<'_> {
    /// `pref`: prefixo dos rótulos internos (`_<pref><i>`); `corpo`: `Sair` volta do `call`.
    fn emitir(&self, prog: &[Passo], pref: &str, corpo: bool, l: &mut Vec<String>) -> ResultType<()> {
        let (app, exe, t) = (self.app, self.exe.as_str(), self.t);
        let log = r#">> "%TROCA%\recuperar.log""#;
        for (i, passo) in prog.iter().enumerate() {
            let k = format!("_{pref}{i}");
            match *passo {
                Passo::Rotulo(r) => l.push(format!(":{r}")),
                Passo::Se { cond, nao, ir } => {
                    let alvo = match cond {
                        Cond::Concluido => format!(r#""%TROCA%\{MARCADOR_CONCLUIDO}""#),
                        Cond::Existe(p) => format!(r#""%{}%""#, p.var()),
                        Cond::ExisteExe(p) => format!(r#""%{}%\{exe}""#, p.var()),
                    };
                    let not = if nao { "not " } else { "" };
                    l.push(format!("if {not}exist {alvo} goto {ir}"));
                }
                Passo::Ir(r) => l.push(format!("goto {r}")),
                Passo::Log(m) => l.push(format!("echo %DATE% %TIME% recuperar {m} {log}")),
                Passo::Aviso(m) => {
                    l.push(format!("echo recuperar: {m}"));
                    l.push(format!("echo %DATE% %TIME% recuperar {m} {log}"));
                }
                Passo::PararServico => {
                    l.push(format!(r#"sc stop "{app}" >nul 2>&1"#));
                    l.push("set /a N=0".into());
                    l.push(format!(":{k}"));
                    l.push(format!(r#"sc query "{app}" | find "STOPPED" >nul && goto {k}_ok"#));
                    l.push(format!("if %N% GEQ {} goto {k}_ok", t.parar));
                    l.push("set /a N+=1".into());
                    l.push("ping -n 2 127.0.0.1 >nul".into());
                    l.push(format!("goto {k}"));
                    l.push(format!(":{k}_ok"));
                    l.extend(self.parar_processos.iter().cloned());
                }
                Passo::Apagar(p) => {
                    l.push(format!(r#"if exist "%{0}%" rd /s /q "%{0}%""#, p.var()))
                }
                Passo::Renomear { de, para, falha } => {
                    l.push(format!(
                        r#"call :ren "%{}%" "{}" "%{}%""#,
                        de.var(),
                        nome_de(self.c.pasta(para))?,
                        para.var()
                    ));
                    l.push(format!("if errorlevel 1 goto {falha}"));
                    l.push(format!(r#"if not exist "%{}%" goto {falha}"#, para.var()));
                }
                Passo::Afastar { falha } => {
                    let nome = nome_de(&self.c.falhou)?;
                    l.push("set /a A=0".into());
                    l.push(format!(":{k}"));
                    l.push("set /a A+=1".into());
                    l.push(format!("if %A% GTR {AFASTADAS} goto {falha}"));
                    l.push(format!(r#"if exist "%FALHOU%.%A%" goto {k}"#));
                    l.push(format!(r#"call :ren "%FALHOU%" "{nome}.%A%" "%FALHOU%.%A%""#));
                    l.push(format!("if errorlevel 1 goto {falha}"));
                    l.push(format!(r#"if not exist "%FALHOU%.%A%" goto {falha}"#));
                    l.push(format!(r#"if exist "%FALHOU%" goto {falha}"#));
                }
                Passo::SubirEstavel { falha } => {
                    l.push(format!(r#"sc start "{app}" >nul 2>&1"#));
                    l.push("set /a N=0".into());
                    l.push(format!(":{k}"));
                    l.push(format!(r#"sc query "{app}" | find "RUNNING" >nul && goto {k}_ok"#));
                    l.push(format!("if %N% GEQ {} goto {falha}", t.subir));
                    l.push("set /a N+=1".into());
                    l.push("ping -n 2 127.0.0.1 >nul".into());
                    l.push(format!("goto {k}"));
                    l.push(format!(":{k}_ok"));
                    l.push("set /a N=0".into());
                    l.push(format!(":{k}_e"));
                    l.push(format!("if %N% GEQ {} goto {k}_fim", t.estavel));
                    l.push("ping -n 2 127.0.0.1 >nul".into());
                    l.push(format!(r#"sc query "{app}" | find "RUNNING" >nul || goto {falha}"#));
                    l.push("set /a N+=1".into());
                    l.push(format!("goto {k}_e"));
                    l.push(format!(":{k}_fim"));
                }
                Passo::MarcarConcluido { falha } => {
                    l.push(format!(r#"echo ok> "%TROCA%\{MARCADOR_CONCLUIDO}""#));
                    l.push(format!(r#"if not exist "%TROCA%\{MARCADOR_CONCLUIDO}" goto {falha}"#));
                }
                Passo::ApagarTarefas => {
                    for tn in self.tarefas.fixas.iter().chain([&self.tarefas.religar]) {
                        l.push(format!(r#"schtasks /Delete /F /TN "{tn}" >nul 2>&1"#));
                    }
                }
                Passo::CriarReligar { falha } => l.push(format!(
                    r#"schtasks /Create /F /RU SYSTEM /RL HIGHEST /SC ONSTART /TN "{}" /TR "\"%TROCA%\{SCRIPT_RECUPERAR}\" religar" >nul 2>&1 || goto {falha}"#,
                    self.tarefas.religar
                )),
                Passo::DesabilitarTarefas { falha } => {
                    // Desabilitou, ou a tarefa não existe (já apagada por uma execução que
                    // concluiu): segue. Existe e não desabilitou: falha.
                    for (j, tn) in self.tarefas.fixas.iter().enumerate() {
                        l.push(format!(
                            r#"schtasks /Change /TN "{tn}" /DISABLE >nul 2>&1 && goto {k}_{j}"#
                        ));
                        l.push(format!(r#"schtasks /Query /TN "{tn}" >nul 2>&1 && goto {falha}"#));
                        l.push(format!(":{k}_{j}"));
                    }
                }
                Passo::HabilitarTarefas { falha } => {
                    // Código de saída do `/ENABLE` não decide: só a consulta confirma. A
                    // consulta lê `Settings.Enabled` (não depende do idioma do Windows); erro
                    // na consulta = não confirmada. Tenta todas e só no fim desvia.
                    let n = t.habilitar.max(1);
                    l.push(r#"set "H=0""#.into());
                    for (j, tn) in self.tarefas.fixas.iter().enumerate() {
                        l.push("set /a N=0".into());
                        l.push(format!(":{k}_{j}"));
                        l.push(format!(r#"schtasks /Change /TN "{tn}" /ENABLE >nul 2>&1"#));
                        l.push(format!("{} >nul 2>&1 && goto {k}_{j}_ok", consulta_habilitada(tn)));
                        l.push("set /a N+=1".into());
                        l.push(format!("if %N% GEQ {n} goto {k}_{j}_nao"));
                        l.push("ping -n 3 127.0.0.1 >nul".into());
                        l.push(format!("goto {k}_{j}"));
                        l.push(format!(":{k}_{j}_nao"));
                        l.push(format!(
                            r#"if not exist "%TROCA%\{MARCADOR_CONCLUIDO}" goto {k}_{j}_falhou"#
                        ));
                        l.push(format!(r#"schtasks /Query /TN "{tn}" >nul 2>&1 || goto {k}_{j}_ok"#));
                        l.push(format!(":{k}_{j}_falhou"));
                        l.push(format!(
                            "echo %DATE% %TIME% recuperar tarefa nao confirmada habilitada {tn} {log}"
                        ));
                        l.push(r#"set "H=1""#.into());
                        l.push(format!(":{k}_{j}_ok"));
                    }
                    l.push(format!(r#"if "%H%"=="1" goto {falha}"#));
                }
                Passo::ComoReabilitar => {
                    let aviso = "ATENCAO a recuperacao automatica NAO foi restabelecida";
                    l.push(format!("echo recuperar: {aviso}"));
                    l.push(format!("echo %DATE% %TIME% recuperar {aviso} {log}"));
                    l.push("echo recuperar: religue a mao como administrador e confira:".into());
                    for tn in self.tarefas.fixas {
                        let cmd = format!(r#"schtasks /Change /TN "{tn}" /ENABLE"#);
                        l.push(format!("echo   {cmd}"));
                        l.push(format!("echo %DATE% %TIME% recuperar religar a mao: {cmd} {log}"));
                        l.push(format!(r#"echo   schtasks /Query /TN "{tn}" /V /FO LIST"#));
                    }
                }
                Passo::ComTrava {
                    esperar,
                    concluida,
                    pendente,
                    ocupada,
                } => {
                    let n = if esperar { t.trava } else { 0 };
                    if n > 0 {
                        l.push("set /a T=0".into());
                        l.push(format!(":{k}"));
                    }
                    l.push(r#"set "SAIDA=3""#.into());
                    l.push(format!(r#"call :principal 9>"%TROCA%\{TRAVA}""#));
                    l.push(format!(r#"if "%SAIDA%"=="0" goto {concluida}"#));
                    l.push(format!(r#"if "%SAIDA%"=="1" goto {pendente}"#));
                    if n > 0 {
                        l.push(format!("if %T% GEQ {n} goto {ocupada}"));
                        l.push("set /a T+=1".into());
                        l.push("ping -n 4 127.0.0.1 >nul".into());
                        l.push(format!("goto {k}"));
                    } else {
                        l.push(format!("goto {ocupada}"));
                    }
                }
                Passo::Sair(x) => {
                    if corpo {
                        l.push(format!(r#"set "SAIDA={}""#, x.codigo()));
                    }
                    l.push(format!("exit /b {}", x.codigo()));
                }
            }
        }
        Ok(())
    }
}

/// Consulta (sem saída) que termina com 0 só se a tarefa existe na raiz da agenda e está
/// habilitada. Caminho absoluto do PowerShell: não depende do PATH da conta SYSTEM.
pub(crate) fn consulta_habilitada(tn: &str) -> String {
    format!(
        r#""%SystemRoot%\System32\WindowsPowerShell\v1.0\powershell.exe" -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "if ((Get-ScheduledTask -TaskPath '\' -TaskName '{tn}' -ErrorAction Stop).Settings.Enabled) {{ exit 0 }} else {{ exit 1 }}""#
    )
}

pub(crate) fn script_de(
    m: &Modos,
    c: &Caminhos,
    app: &str,
    broker: &str,
    tarefas: &Tarefas,
    t: &Tempos,
) -> ResultType<String> {
    for v in [app, broker, tarefas.religar].iter().chain(tarefas.fixas) {
        if !seguro_para_cmd(v) {
            bail!("nome inválido para o cmd: {v:?}");
        }
    }
    // As fixas também vão entre aspas simples na consulta do PowerShell, cujo `-TaskName`
    // aceita curingas: nada de aspas, crase nem `*?[]`.
    if let Some(v) = tarefas.fixas.iter().find(|v| v.chars().any(|ch| "'`*?[]$".contains(ch))) {
        bail!("nome de tarefa inválido para o PowerShell: {v:?}");
    }
    if tarefas.fixas.len() != TAREFAS_FIXAS {
        bail!("esperava {TAREFAS_FIXAS} tarefas fixas, vieram {}", tarefas.fixas.len());
    }
    validar_modos(m)?;
    let exe = format!("{app}.exe");
    let g = Gerador {
        c,
        app,
        parar_processos: [
            format!(r#"taskkill /F /IM "{exe}" >nul 2>&1"#),
            format!(r#"taskkill /F /IM "{broker}" >nul 2>&1"#),
        ],
        exe,
        tarefas,
        t,
    };
    let mut l: Vec<String> = Vec::new();
    l.push("@echo off".into());
    l.push("setlocal EnableExtensions DisableDelayedExpansion".into());
    l.push(r#"cd /d "%SystemRoot%""#.into());
    for p in Pasta::TODAS {
        l.push(format!(r#"set "{}={}""#, p.var(), texto(c.pasta(p))?));
    }
    l.push(format!(r#"set "TROCA={}""#, texto(&c.troca)?));
    l.push(r#"if /i "%~1"=="manual" goto manual"#.into());
    l.push(r#"if /i "%~1"=="religar" goto religar"#.into());
    l.push(r#"if not "%~1"=="" exit /b 2"#.into());
    g.emitir(&m.automatico, "a", false, &mut l)?;
    l.push(":manual".into());
    g.emitir(&m.manual, "m", false, &mut l)?;
    l.push(":religar".into());
    g.emitir(&m.religar, "r", false, &mut l)?;
    l.push(":principal".into());
    g.emitir(&m.corpo, "s", true, &mut l)?;
    // :ren <de> <novo nome> <destino completo>: o destino não pode existir.
    l.push(":ren".into());
    l.push(r#"if exist "%~3" exit /b 1"#.into());
    l.push("set /a R=0".into());
    l.push(":ren_loop".into());
    l.push(r#"ren "%~1" "%~2" >nul 2>&1 && exit /b 0"#.into());
    l.push("set /a R+=1".into());
    l.push(format!("if %R% GEQ {} exit /b 1", t.renomear.max(1)));
    l.extend(g.parar_processos.iter().cloned());
    l.push("ping -n 4 127.0.0.1 >nul".into());
    l.push("goto ren_loop".into());
    let mut s = l.join("\r\n");
    s.push_str("\r\n");
    Ok(s)
}

/// `guarda.bat`: espera o processo da troca (`pid`) terminar; só então, sem `concluido`,
/// roda `recuperar.bat`. Limite de `ciclos`×5 s: aí encerra o processo e recupera.
pub(crate) fn script_guarda(c: &Caminhos, pid: u32, ciclos: u32) -> ResultType<String> {
    let troca = texto(&c.troca)?;
    Ok(format!(
        r#"@echo off
setlocal EnableExtensions DisableDelayedExpansion
cd /d "%SystemRoot%"
set "TROCA={troca}"
set /a N=0
:espera
if exist "%TROCA%\{concluido}" exit /b 0
tasklist /FI "PID eq {pid}" /NH 2>nul | find " {pid} " >nul || goto recuperar
set /a N+=1
if %N% GEQ {ciclos} goto limite
ping -n 6 127.0.0.1 >nul
goto espera
:limite
taskkill /F /PID {pid} >nul 2>&1
ping -n 3 127.0.0.1 >nul
:recuperar
if exist "%TROCA%\{concluido}" exit /b 0
call "%TROCA%\recuperar.bat"
exit /b 0
"#,
        concluido = MARCADOR_CONCLUIDO,
    )
    .replace('\n', "\r\n"))
}

/// Implementação real no Windows: SCM via `windows-service`, `taskkill` e `rename`.
#[cfg(windows)]
pub(crate) struct SistemaWindows {
    pub servico: String,
    /// Imagens a encerrar (ex.: `SuporteVancin.exe`, o broker), menos este processo.
    pub imagens: Vec<String>,
    /// Tarefas agendadas da recuperação.
    pub tarefas: Vec<String>,
}

#[cfg(windows)]
impl SistemaWindows {
    fn abrir(
        &self,
        acesso: windows_service::service::ServiceAccess,
    ) -> windows_service::Result<windows_service::service::Service> {
        use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)?
            .open_service(&self.servico, acesso)
    }
}

#[cfg(windows)]
impl Sistema for SistemaWindows {
    fn servico(&mut self) -> Servico {
        use windows_service::service::{ServiceAccess, ServiceState};
        match self
            .abrir(ServiceAccess::QUERY_STATUS)
            .and_then(|s| s.query_status())
            .map(|st| st.current_state)
        {
            Ok(ServiceState::Running) => Servico::Rodando,
            Ok(ServiceState::Stopped) => Servico::Parado,
            Ok(_) => Servico::Transicao,
            Err(e) => {
                log::warn!("[portal-update] consulta do serviço: {}", e);
                Servico::Desconhecido
            }
        }
    }
    fn parar_servico(&mut self) {
        use windows_service::service::ServiceAccess;
        if let Err(e) = self.abrir(ServiceAccess::STOP).and_then(|s| s.stop().map(|_| ())) {
            log::info!("[portal-update] parar serviço: {}", e);
        }
    }
    fn iniciar_servico(&mut self) {
        use windows_service::service::ServiceAccess;
        let vazio: [&std::ffi::OsStr; 0] = [];
        if let Err(e) = self.abrir(ServiceAccess::START).and_then(|s| s.start(&vazio[..])) {
            // ERROR_SERVICE_ALREADY_RUNNING também cai aqui; o estado é conferido depois.
            log::info!("[portal-update] iniciar serviço: {}", e);
        }
    }
    fn encerrar_processos(&mut self) {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let pid = std::process::id();
        let filtro = format!("PID ne {pid}");
        for img in &self.imagens {
            let _ = std::process::Command::new("taskkill")
                .args(["/F", "/IM", img.as_str(), "/FI", filtro.as_str()])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
        }
    }
    fn renomear(&mut self, de: &Path, para: &Path) -> io::Result<()> {
        if para.exists() {
            return Err(io::Error::new(io::ErrorKind::AlreadyExists, "destino existe"));
        }
        fs::rename(de, para)
    }
    fn esperar(&mut self, d: Duration) {
        std::thread::sleep(d);
    }
    fn apagar_tarefas(&mut self) {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        for t in &self.tarefas {
            let r = std::process::Command::new("schtasks")
                .args(["/Delete", "/F", "/TN", t.as_str()])
                .creation_flags(CREATE_NO_WINDOW)
                .output();
            if !matches!(r, Ok(ref o) if o.status.success()) {
                log::warn!("[portal-update] não apagou a tarefa {t}");
            }
        }
    }
}

/// Roda `exe --suporte-build-info <arquivo>` (carrega EXE + DLLs da pasta dele) e confere o
/// relatório contra o esperado. Qualquer falha (não carrega, trava, outra build) = Err.
#[cfg(windows)]
pub(crate) fn validar_exe(exe: &Path, relatorio: &Path, esperado: &str, limite: Duration) -> ResultType<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let _ = fs::remove_file(relatorio);
    let mut filho = std::process::Command::new(exe)
        .arg("--suporte-build-info")
        .arg(relatorio)
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| anyhow!("EXE novo não executa: {e}"))?;
    let inicio = std::time::Instant::now();
    let status = loop {
        if let Some(st) = filho.try_wait()? {
            break st;
        }
        if inicio.elapsed() > limite {
            let _ = filho.kill();
            let _ = filho.wait();
            bail!("EXE novo não respondeu em {:?}", limite);
        }
        std::thread::sleep(Duration::from_millis(200));
    };
    if !status.success() {
        bail!("EXE novo saiu com {status}");
    }
    let t = fs::read_to_string(relatorio).map_err(|e| anyhow!("EXE novo sem relatório: {e}"))?;
    let info = super::portal::parse_build_info(&t)?;
    super::portal::conferir_esperado(&info, esperado)
}

#[cfg(test)]
mod tests;

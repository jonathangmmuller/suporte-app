//! Troca da instalação do Windows com volta garantida (app de suporte).
//!
//! Requisito: a máquina nunca pode ficar inacessível; ID, senha permanente e configuração
//! (perfil do serviço, fora da pasta de instalação) não são tocados.
//!
//! Desenho (pastas irmãs da instalação, no mesmo volume, para `rename` atômico):
//!   `<inst>.novo`      nova instalação montada e conferida ANTES de parar o serviço
//!   `<inst>.anterior`  instalação anterior, íntegra (é só renomeada, nunca sobrescrita)
//!   `<inst>.falhou`    nova instalação que não subiu (fica para diagnóstico)
//!   `<inst>.troca`     scripts da guarda/recuperação e o marcador `concluido`
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
//!    então, se não houver `concluido`, roda `recuperar.bat` (mesma tabela de `recuperar`).
//!    Uma tarefa agendada "ao iniciar" roda o mesmo `recuperar.bat` se a máquina reiniciar no
//!    meio. A guarda nunca age enquanto a troca está em andamento (espera o PID sair).

use hbb_common::{anyhow::anyhow, bail, log, ResultType};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

pub(crate) const MARCADOR_CONCLUIDO: &str = "concluido";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Caminhos {
    pub atual: PathBuf,
    pub novo: PathBuf,
    pub anterior: PathBuf,
    pub falhou: PathBuf,
    pub troca: PathBuf,
}

impl Caminhos {
    pub fn de(atual: &Path) -> Self {
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
            troca: irmao(".troca"),
        }
    }

    pub fn concluido(&self) -> PathBuf {
        self.troca.join(MARCADOR_CONCLUIDO)
    }

    /// Apaga restos de uma atualização anterior (já concluída: o serviço que nos chamou roda
    /// da instalação atual). Nunca toca a instalação atual.
    pub fn limpar_restos(&self) -> ResultType<()> {
        for d in [&self.novo, &self.anterior, &self.falhou, &self.troca] {
            remover_arvore(d)?;
        }
        Ok(())
    }
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
    fs::create_dir_all(&c.novo)?;
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
    fn renomear(&mut self, de: &Path, para: &Path) -> io::Result<()>;
    fn esperar(&mut self, d: Duration);
}

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
}

impl Tempos {
    /// Pior caso ≈ 60 + 3×(20×3) + 60 + 20 + (60 + 60) s ≈ 8 min; a guarda espera até 20 min.
    pub const PADRAO: Tempos = Tempos {
        passo: Duration::from_secs(1),
        parar: 60,
        subir: 60,
        estavel: 20,
        renomear: 20,
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

fn garantir_servico(s: &mut dyn Sistema, t: &Tempos) -> bool {
    s.iniciar_servico();
    esperar_estado(s, Servico::Rodando, t.subir, t)
}

/// Troca `c.atual` por `c.novo` (já preparado e conferido). `religar`: o serviço estava
/// rodando antes e precisa estar rodando depois.
pub(crate) fn trocar(c: &Caminhos, s: &mut dyn Sistema, religar: bool, t: &Tempos) -> Resultado {
    s.parar_servico();
    if !esperar_estado(s, Servico::Parado, t.parar, t) {
        log::warn!("[portal-update] serviço não parou a tempo; encerrando processos");
    }
    s.encerrar_processos();

    if let Err(e) = renomear_tentando(s, &c.atual, &c.anterior, t.renomear, t) {
        // Nada mudou no disco.
        if religar {
            garantir_servico(s, t);
        }
        return Resultado::NaoTrocado(format!("instalação atual presa: {e}"));
    }
    if let Err(e) = renomear_tentando(s, &c.novo, &c.atual, t.renomear, t) {
        return match renomear_tentando(s, &c.anterior, &c.atual, t.renomear * 3, t) {
            Ok(()) => {
                if religar {
                    garantir_servico(s, t);
                }
                Resultado::NaoTrocado(format!("nova instalação não entrou no lugar: {e}"))
            }
            Err(e2) => Resultado::Critico(format!(
                "nova não entrou ({e}) e a anterior não voltou ({e2})"
            )),
        };
    }
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
        garantir_servico(s, t);
        return Resultado::Critico(format!("versão nova não subiu e não saiu do lugar: {e}"));
    }
    if let Err(e) = renomear_tentando(s, &c.anterior, &c.atual, t.renomear * 3, t) {
        // Não deixa a pasta vazia: devolve a nova e tenta subir.
        let _ = renomear_tentando(s, &c.falhou, &c.atual, t.renomear * 3, t);
        garantir_servico(s, t);
        return Resultado::Critico(format!("anterior não voltou: {e}"));
    }
    if garantir_servico(s, t) {
        Resultado::Revertido("serviço da versão nova não ficou RUNNING".to_owned())
    } else {
        Resultado::Critico("anterior restaurada mas o serviço não subiu".to_owned())
    }
}

/// Fluxo completo: prepara e confere `.novo`, prepara a guarda (`antes_de_parar`) e só então
/// troca. Falha antes da troca não toca o serviço nem a instalação atual.
pub(crate) fn atualizar(
    c: &Caminhos,
    p: &Preparo,
    copiar: &mut dyn FnMut(&Path, &Path) -> io::Result<()>,
    validar: &mut dyn FnMut(&Path) -> ResultType<()>,
    antes_de_parar: &mut dyn FnMut() -> ResultType<()>,
    s: &mut dyn Sistema,
    religar: bool,
    t: &Tempos,
) -> Resultado {
    let antes = preparar(c, p, copiar).and_then(|n| {
        log::info!("[portal-update] nova instalação montada e conferida ({n} arquivos)");
        validar(&c.novo.join(p.exe_destino))?;
        antes_de_parar()
    });
    if let Err(e) = antes {
        if let Err(e2) = remover_arvore(&c.novo) {
            log::warn!("[portal-update] não consegui apagar {:?}: {}", c.novo, e2);
        }
        return Resultado::NaoTrocado(format!("antes de parar o serviço: {e:#}"));
    }
    trocar(c, s, religar, t)
}

/// Tabela de decisão da recuperação (a mesma do `recuperar.bat`), para quando a troca foi
/// interrompida: com `concluido` não faz nada além de garantir o serviço.
pub(crate) fn recuperar(c: &Caminhos, exe: &str, s: &mut dyn Sistema, t: &Tempos) -> bool {
    if !c.concluido().exists() && c.anterior.join(exe).is_file() {
        s.parar_servico();
        esperar_estado(s, Servico::Parado, t.parar, t);
        s.encerrar_processos();
        if c.atual.exists() {
            let _ = remover_arvore(&c.falhou);
            if let Err(e) = renomear_tentando(s, &c.atual, &c.falhou, t.renomear, t) {
                log::error!("[portal-update] recuperação: {:?} presa: {}", c.atual, e);
            }
        }
        if !c.atual.exists() {
            if let Err(e) = renomear_tentando(s, &c.anterior, &c.atual, t.renomear, t) {
                log::error!("[portal-update] recuperação: anterior não voltou: {}", e);
            }
        }
    }
    garantir_servico(s, t)
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

/// `recuperar.bat`: transcrição de `recuperar` para o cmd (roda pela guarda ou pela tarefa
/// agendada ao iniciar, como SYSTEM). Usa `ren` (falha se o destino existir; `move` aninharia).
pub(crate) fn script_recuperar(c: &Caminhos, app: &str, broker: &str, tarefa: &str) -> ResultType<String> {
    for v in [app, broker, tarefa] {
        if !seguro_para_cmd(v) {
            bail!("nome inválido para o cmd: {v:?}");
        }
    }
    let atual = texto(&c.atual)?;
    let anterior = texto(&c.anterior)?;
    let falhou = texto(&c.falhou)?;
    let troca = texto(&c.troca)?;
    let nome_atual = nome_de(&c.atual)?;
    let nome_falhou = nome_de(&c.falhou)?;
    Ok(format!(
        r#"@echo off
setlocal EnableExtensions DisableDelayedExpansion
cd /d "%SystemRoot%"
set "ATUAL={atual}"
set "ANTERIOR={anterior}"
set "FALHOU={falhou}"
set "TROCA={troca}"
echo %DATE% %TIME% recuperar inicio>> "%TROCA%\recuperar.log"
if exist "%TROCA%\{concluido}" goto garantir
if not exist "%ANTERIOR%\{app}.exe" goto garantir
sc stop "{app}" >nul 2>&1
set /a N=0
:parar
sc query "{app}" | find "STOPPED" >nul && goto parado
set /a N+=1
if %N% GEQ 30 goto parado
ping -n 2 127.0.0.1 >nul
goto parar
:parado
taskkill /F /IM "{app}.exe" >nul 2>&1
taskkill /F /IM "{broker}" >nul 2>&1
if exist "%ATUAL%" if exist "%FALHOU%" rd /s /q "%FALHOU%"
if exist "%ATUAL%" call :ren "%ATUAL%" "{nome_falhou}"
if not exist "%ATUAL%" call :ren "%ANTERIOR%" "{nome_atual}"
echo %DATE% %TIME% recuperar trocou>> "%TROCA%\recuperar.log"
:garantir
sc start "{app}" >nul 2>&1
schtasks /Delete /F /TN "{tarefa}" >nul 2>&1
echo ok> "%TROCA%\{concluido}"
echo %DATE% %TIME% recuperar fim>> "%TROCA%\recuperar.log"
exit /b 0
:ren
set /a R=0
:ren_loop
ren "%~1" "%~2" >nul 2>&1 && exit /b 0
set /a R+=1
if %R% GEQ 20 exit /b 1
taskkill /F /IM "{app}.exe" >nul 2>&1
taskkill /F /IM "{broker}" >nul 2>&1
ping -n 4 127.0.0.1 >nul
goto ren_loop
"#,
        concluido = MARCADOR_CONCLUIDO,
    )
    .replace('\n', "\r\n"))
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

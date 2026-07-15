// ЧЕРНОВИК / СКЕЛЕТ (svitlo-store ProminBackend, L1.2)
//
// serde-структуры под стабильный JSON-выход `promin --json <cmd>` (CL=A).
// Формы выверены по promin/client/__main__.py и promin/client/client.py
// (ревизия ed693cf). Если контракт client меняется — править ЗДЕСЬ, не в backend.
//
// ВЫВЕРИТЬ В СРЕДЕ:
//   * lock.json / config.json раскладку на реальном образе (пути ниже)
//   * поведение `install` при стриме прогресса (NDJSON ещё не реализован, см
//     PROMIN-BACKEND.md «Открытые узлы» п.1) — сейчас одно событие по завершении

use serde::Deserialize;
use std::collections::BTreeMap;

/// Общий конверт ошибки: при `--json` любой сбой client отдаёт `{ok:false,error}`
/// (см __main__.py except-ветку). Успешные команды НЕ несут `ok` кроме install/remove.
#[derive(Debug, Clone, Deserialize)]
pub struct ProminError {
    pub ok: bool,
    pub error: String,
}

/// Универсальный разбор: пробуем ошибку, потом полезную форму.
/// client при `--json` пишет ровно одну JSON-строку в stdout.
pub fn parse_or_err<T: for<'de> Deserialize<'de>>(stdout: &str) -> Result<T, String> {
    let line = stdout.trim();
    if line.is_empty() {
        return Err("promin: пустой вывод".to_string());
    }
    // Сначала пробуем структурную ошибку {ok:false,error}
    if let Ok(e) = serde_json::from_str::<ProminError>(line) {
        if !e.ok {
            return Err(e.error);
        }
    }
    serde_json::from_str::<T>(line).map_err(|e| format!("promin: разбор JSON: {e} (вход: {line})"))
}

// ---- search: `--json search <q>` -> {query, results:[name...]} ----
#[derive(Debug, Clone, Deserialize)]
pub struct SearchResult {
    #[allow(dead_code)]
    pub query: String,
    pub results: Vec<String>,
}

// ---- recipes: `--json recipes` -> {recipes:[name...]} ----
#[derive(Debug, Clone, Deserialize)]
pub struct RecipesResult {
    pub recipes: Vec<String>,
}

// ---- accept-eula: `--json accept-eula <channel>` -> {ok, channel, eula_accepted} ----
// Gated/nonfree-каналы гейтят install по EULA (client падает client_eula_required без
// маркера). Принятие фиксирует маркер /promin/eula/<channel>.accepted. В шиппинг-сторе
// стандартного канала не срабатывает (EULA только у gated). eula_accepted = путь маркера.
#[derive(Debug, Clone, Deserialize)]
pub struct AcceptEulaResult {
    #[allow(dead_code)]
    pub ok: bool,
    #[allow(dead_code)]
    pub channel: String,
    #[allow(dead_code)]
    pub eula_accepted: String,
}

// ---- list: `--json list` -> {packages:{name:store_name}} ----
// store_name это basename store-пути (см __main__.py: Path(pa).name)
#[derive(Debug, Clone, Deserialize)]
pub struct ListResult {
    pub packages: BTreeMap<String, String>,
}

// ---- status: `--json status` ----
// {release, current_generation, generations:[], servers:[]|null, offline,
//  lock:bool, lock_packages:int|null}
#[derive(Debug, Clone, Deserialize)]
pub struct StatusResult {
    pub release: String,
    pub current_generation: i64,
    #[serde(default)]
    pub generations: Vec<i64>,
    #[serde(default)]
    pub servers: Option<Vec<String>>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default)]
    pub lock: bool,
    #[serde(default)]
    pub lock_packages: Option<u64>,
}

// ---- generations: `--json generations` -> {current, generations:[]} ----
#[derive(Debug, Clone, Deserialize)]
pub struct GenerationsResult {
    pub current: i64,
    #[serde(default)]
    pub generations: Vec<i64>,
}

// ---- install: `--json install <pkg>` -> {ok:true, installed} | {ok:false,error} ----
#[derive(Debug, Clone, Deserialize)]
pub struct InstallResult {
    pub ok: bool,
    #[serde(default)]
    pub installed: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

// ---- remove: `--json remove <pkg>` -> {ok:true, removed} | {ok:false,error} ----
#[derive(Debug, Clone, Deserialize)]
pub struct RemoveResult {
    pub ok: bool,
    #[serde(default)]
    pub removed: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

// =====================================================================
// Файлы образа, читаемые НАПРЯМУЮ (не через CLI): lock.json + config.json
// =====================================================================

/// /promin/lock.json — пин релиза (L1/S3/CH). Несёт hash-pin всех пакетов
/// профиля релиза. Источник истины «что доступно/что обновилось».
/// Поля по client.py::_install_locked / _load_lock / _check_promin_ref.
#[derive(Debug, Clone, Deserialize)]
pub struct Lock {
    #[serde(default)]
    pub release: Option<String>,
    #[serde(default)]
    pub promin_ref: Option<String>,
    #[serde(default)]
    pub package_count: Option<u64>,
    /// карта name -> запись пина
    #[serde(default)]
    pub packages: BTreeMap<String, LockEntry>,
}

/// Запись пакета в lock: версия + input_hash (store_name выводится промином
/// из них) + runtime-замыкание. store_name НЕ в lock — собирается промином,
/// нам он нужен лишь для сверки «установлено == залочено» через list.
#[derive(Debug, Clone, Deserialize)]
pub struct LockEntry {
    pub version: String,
    pub input_hash: String,
    #[serde(default)]
    pub runtime_deps: Vec<String>,
    /// store_name появляется в некоторых дампах lock; если есть — используем
    /// напрямую для сравнения с `list`, иначе строим <hash>-<name>-<version>
    #[serde(default)]
    pub store_name: Option<String>,
}

impl LockEntry {
    /// Реконструкция store_name по конвенции promin /promin/store/<hash>-<name>-<version>
    /// (см CLAUDE.md инвариант store-путей). ВЫВЕРИТЬ точный формат basename:
    /// __main__.py отдаёт Path(pa).name — это <hash>-<name>-<version> без префикса пути.
    pub fn store_name(&self, name: &str) -> String {
        match &self.store_name {
            Some(s) => s.clone(),
            None => format!("{}-{}-{}", self.input_hash, name, self.version),
        }
    }
}

/// /promin/config.json — каналы/серверы клиента (clientconfig.py::DEFAULT).
#[derive(Debug, Clone, Deserialize)]
pub struct ProminConfig {
    #[serde(default = "default_servers")]
    pub servers: Vec<String>,
    #[serde(default)]
    pub offline: bool,
    #[serde(default = "default_timeout")]
    pub timeout: u64,
    /// канал (версия релиза); пусто = legacy recompute
    #[serde(default)]
    pub release: String,
}

fn default_servers() -> Vec<String> {
    vec!["https://cache.svitlolinux.org".to_string()]
}
fn default_timeout() -> u64 {
    30
}

impl Default for ProminConfig {
    fn default() -> Self {
        Self {
            servers: default_servers(),
            offline: false,
            timeout: default_timeout(),
            release: String::new(),
        }
    }
}

// ---- прогресс install (БУДУЩЕЕ, NDJSON) ----
// promin сейчас НЕ стримит (PROMIN-BACKEND.md «Открытые узлы» п.1: результат по
// завершении). Структура заведена под эволюцию контракта к NDJSON-строкам
// `{event, package, fraction}` — backend готов их парсить построчно.
// ВЫВЕРИТЬ: формат когда client начнёт стримить (--json install).
#[derive(Debug, Clone, Deserialize)]
pub struct ProgressEvent {
    /// "fetch" | "build" | "activate" | "done" | "error"
    pub event: String,
    #[serde(default)]
    pub package: Option<String>,
    /// 0.0..=1.0
    #[serde(default)]
    pub fraction: Option<f32>,
    #[serde(default)]
    pub error: Option<String>,
}

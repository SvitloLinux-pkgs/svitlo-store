// ProminBackend (svitlo-store, L1.2 / L1.3 / L2)
//
// Реализация cosmic-store `Backend` trait поверх promin client JSON-API (CL=A).
// Слой ДАННЫХ форка — UI/AppStream/локали апстрима как есть, меняется только
// источник пакетов: subprocess `promin --json <cmd>` вместо PackageKit/Flatpak.
// Шаблон сигнатур снят с upstream src/backend/packagekit.rs (выверено по
// вендоренному дереву cosmic-store, см ниже PIN).
//
// Контракт client выверен по promin/client/__main__.py + client.py (ed693cf):
//   search/status/list/generations/install/remove/rollback/recipes/gc.
// Файлы образа читаются напрямую: /promin/lock.json (доступность+updates),
//   /promin/config.json (каналы).
//
// ВЫВЕРЕНО ПО ВЕНДОРЕННОМУ cosmic-store (svitlo-data/cosmic-src/.../cosmic-store):
//   * trait Backend / Package / Operation / OperationKind / AppInfo / AppId /
//     AppstreamCache::new совпадают с тем что использует этот модуль (cosmic-epoch
//     текущего набора). PIN ревизии всё равно держать в рецепте (source.branch)
//   * команда запуска: образ кладёт /usr/bin/promin = sh-обёртка
//     `python3 -m promin.client --prefix /promin "$@"` (svitlo/stages/image.py).
//     Поэтому PROMIN_BIN="promin" верен. Обёртка УЖЕ подставляет --prefix /promin,
//     наш повторный --prefix перетирается argparse'ом (last-wins) — безвреден.
//     При нестандартном prefix (тесты) зовём бинарь промина напрямую, не обёртку
//   * прогресс install: client НЕ стримит (одно событие по завершении). Шкала f()
//     это 0..100 (потребитель в main.rs делит /100.0), НЕ 0..1. Делим по числу
//     пакетов. NDJSON-эволюция готова в json_types::ProgressEvent
//   * эскалация привилегий install/remove (L3.1): зовём напрямую, в образе install
//     пишет в /promin/store (группа promin) + меняет поколение. Из GUI под юзером
//     нужен pkexec/polkit или D-Bus сервис, см TODO в operation()

use cosmic::widget;
use std::collections::HashMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::backend::{appstream, json_types as jt, Backend, Package};
use crate::{AppId, AppInfo, AppKind, AppstreamCache, GStreamerCodec, Operation, OperationKind};

/// Бинарь client в установленной системе (sh-обёртка `python3 -m promin.client`,
/// кладётся образом в /usr/bin/promin, см svitlo/stages/image.py).
const PROMIN_BIN: &str = "promin";

#[derive(Debug)]
pub struct ProminBackend {
    /// локаль UI; держим для пересборки кеша при сетевом префетче каталога
    /// по локали (D-APPSTREAM, ещё не реализован) — пока читается только в new()
    #[allow(dead_code)]
    locale: String,
    /// корень promin (/promin): lock.json, config.json
    prefix: PathBuf,
    /// разобранный config.json (каналы/серверы)
    config: jt::ProminConfig,
    /// lock релиза (доступность пакетов + hash-pin для updates); None = legacy
    lock: Option<jt::Lock>,
    /// один AppStream-кеш на промин-канал (как packagekit держит свой)
    appstream_caches: Vec<AppstreamCache>,
}

impl ProminBackend {
    pub fn new(locale: &str) -> Result<Self, Box<dyn Error>> {
        Self::with_prefix(locale, Path::new("/promin"))
    }

    pub fn with_prefix(locale: &str, prefix: &Path) -> Result<Self, Box<dyn Error>> {
        let config = read_json::<jt::ProminConfig>(&prefix.join("config.json")).unwrap_or_default();
        let lock = read_json::<jt::Lock>(&prefix.join("lock.json"));
        let release = if config.release.is_empty() {
            lock.as_ref().and_then(|l| l.release.clone()).unwrap_or_default()
        } else {
            config.release.clone()
        };
        Ok(Self {
            locale: locale.to_string(),
            prefix: prefix.to_path_buf(),
            config,
            lock,
            appstream_caches: vec![appstream::build_cache(locale, &release)],
        })
    }

    fn cache(&self) -> &AppstreamCache {
        &self.appstream_caches[0]
    }

    /// Запуск `promin [--prefix P] --json <args...>`, возврат stdout.
    /// Ошибки процесса/непулевой код -> Err. Сам JSON-разбор у вызывающего.
    fn run_json(&self, args: &[&str]) -> Result<String, Box<dyn Error>> {
        let mut cmd = Command::new(PROMIN_BIN);
        // явный prefix чтобы тесты/нестандартный корень работали
        cmd.arg("--prefix").arg(&self.prefix).arg("--json");
        cmd.args(args);
        let out = cmd
            .output()
            .map_err(|e| format!("не запустить {PROMIN_BIN}: {e}"))?;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.status.success() {
            // client при --json кладёт {ok:false,error} в stdout даже при коде 1
            if let Some(line) = stdout.lines().next() {
                if let Ok(e) = serde_json::from_str::<jt::ProminError>(line.trim()) {
                    if !e.ok {
                        return Err(e.error.into());
                    }
                }
            }
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(format!("{PROMIN_BIN} {:?} код {:?}: {stderr}", args, out.status.code()).into());
        }
        Ok(stdout)
    }

    /// Строит Package из имени промин-пакета, обогащая метаданными каталога.
    /// Если каталог знает компонент(ы) для pkgname — отдаём их (как packagekit);
    /// иначе generic-карточка (имя=slug, иконка package-x-generic).
    fn packages_for(&self, name: &str, version: String) -> Vec<Package> {
        let cache = self.cache();
        let ids = appstream::ids_for_pkg(cache, name);
        if ids.is_empty() {
            return vec![self.generic_package(name, version)];
        }
        let mut out = Vec::new();
        for id in ids {
            match cache.infos.get(id) {
                Some(info) => out.push(Package {
                    id: id.clone(),
                    icon: cache.icon(info),
                    info: info.clone(),
                    version: version.clone(),
                    extra: HashMap::new(),
                }),
                None => {
                    log::warn!("promin: каталог без info для {:?}", id);
                    out.push(self.generic_package(name, version.clone()));
                }
            }
        }
        out
    }

    /// Generic-карточка для пакета без AppStream MetaInfo (CLI/шрифт/либа).
    /// pkgnames=[name] чтобы operation() мог сослаться на промин-имя.
    fn generic_package(&self, name: &str, version: String) -> Package {
        let cache = self.cache();
        Package {
            id: AppId::new(name),
            icon: widget::icon::from_name("package-x-generic").size(128).handle(),
            info: Arc::new(AppInfo {
                source_id: cache.source_id.clone(),
                source_name: cache.source_name.clone(),
                name: name.to_string(),
                pkgnames: vec![name.to_string()],
                ..Default::default()
            }),
            version,
            extra: HashMap::new(),
        }
    }

    /// `--json list` -> map name->store_name (basename store-пути).
    fn installed_map(&self) -> Result<jt::ListResult, Box<dyn Error>> {
        let out = self.run_json(&["list"])?;
        jt::parse_or_err::<jt::ListResult>(&out).map_err(Into::into)
    }

    /// Принять EULA gated/nonfree-канала: `promin --json accept-eula <channel>`.
    /// UI зовёт когда install из gated-канала падает client_eula_required (показать
    /// EULA-текст -> согласие пользователя -> этот вызов -> повтор install). В шиппинг-сторе
    /// стандартного канала НЕ срабатывает (EULA только у gated, nonfree ещё не в наборе).
    #[allow(dead_code)]
    pub fn accept_eula(&self, channel: &str) -> Result<jt::AcceptEulaResult, Box<dyn Error>> {
        let out = self.run_json(&["accept-eula", channel])?;
        jt::parse_or_err::<jt::AcceptEulaResult>(&out).map_err(Into::into)
    }

    /// `--json status` (release/generation/lock-флаги).
    fn status(&self) -> Result<jt::StatusResult, Box<dyn Error>> {
        let out = self.run_json(&["status"])?;
        jt::parse_or_err::<jt::StatusResult>(&out).map_err(Into::into)
    }

    /// Имена промин-пакетов из набора infos операции (как packagekit берёт pkgnames).
    fn op_pkg_names(op: &Operation) -> Vec<String> {
        let mut names = Vec::new();
        for info in op.infos.iter() {
            for p in &info.pkgnames {
                names.push(p.clone());
            }
        }
        names
    }

    /// `--json recipes` -> весь индекс доступных промин-пакетов (slug'и).
    /// Это полнота витрины: пакеты без AppStream MetaInfo (CLI/шрифты/либы) тоже
    /// должны быть видны/искаемы. Ошибку глотаем (offline/нет client) -> пусто.
    fn all_recipes(&self) -> Vec<String> {
        match self.run_json(&["recipes"]) {
            Ok(out) => jt::parse_or_err::<jt::RecipesResult>(&out)
                .map(|r| r.recipes)
                .unwrap_or_default(),
            Err(e) => {
                log::info!("promin: --json recipes недоступен ({e}), витрина только из AppStream");
                Vec::new()
            }
        }
    }

    /// Достроить AppstreamCache промин-индексом: для каждого промин-пакета, у
    /// которого в каталоге НЕТ компонента, кладём generic AppInfo (id=slug,
    /// pkgname=slug, категория из YAML-fallback если есть). Так upstream-UI
    /// (поиск/категории/карточки) показывает ВЕСЬ promin-канал, не только пакеты
    /// с MetaInfo. Пакеты с MetaInfo трогать НЕ нужно — у них уже есть запись.
    ///
    /// Источник индекса: `--json recipes` (полный) пересечённый с lock релиза
    /// (если lock есть — показываем только то что в канале доступно binary-first).
    fn augment_cache_with_recipes(&mut self) {
        let recipes = self.all_recipes();
        if recipes.is_empty() {
            return;
        }
        // если есть lock канала — ограничиваем витрину доступным в релизе
        let in_channel = |n: &str| -> bool {
            match &self.lock {
                Some(l) if !l.packages.is_empty() => l.packages.contains_key(n),
                _ => true,
            }
        };
        let cache = &mut self.appstream_caches[0];
        for name in recipes {
            if !in_channel(&name) {
                continue;
            }
            // у пакета уже есть компонент в каталоге -> не дублируем
            if cache.pkgnames.contains_key(&name) {
                continue;
            }
            let id = AppId::new(&name);
            let info = Arc::new(AppInfo {
                source_id: cache.source_id.clone(),
                source_name: cache.source_name.clone(),
                name: name.clone(),
                pkgnames: vec![name.clone()],
                // Витрина показывает ПРИЛОЖЕНИЯ, не библиотеки. Сюда попадают пакеты БЕЗ
                // компонента в каталоге (проверка in_channel/pkgnames выше), а каталог
                // генерится ТОЛЬКО для рецептов с .desktop/metainfo (app-ness авторитетна по
                // членству в каталоге). Значит augmented-пакет по определению НЕ приложение
                // (нет desktop-entry) -> метим Addon, а не дефолтный DesktopApplication.
                // main.rs фильтрует поиск/категории по DesktopApplication, поэтому Addon НЕ
                // засоряет витрину либами, но остаётся в индексе для install-by-name бэкендом.
                kind: AppKind::Addon,
                ..Default::default()
            });
            cache.infos.insert(id.clone(), info);
            cache
                .pkgnames
                .entry(name)
                .or_default()
                .insert(id);
        }
    }
}

impl Backend for ProminBackend {
    fn load_caches(&mut self, refresh: bool) -> Result<(), Box<dyn Error>> {
        if refresh {
            // перечитать config/lock (могли смениться каналом/синком)
            if let Some(c) = read_json::<jt::ProminConfig>(&self.prefix.join("config.json")) {
                self.config = c;
            }
            self.lock = read_json::<jt::Lock>(&self.prefix.join("lock.json"));
            // диагностика канала (release/lock/generation) — помогает понять почему
            // витрина/updates пусты (нет lock => нет канала, см updates())
            match self.status() {
                Ok(st) => log::info!(
                    "promin: канал release={:?} gen={} lock={} lock_packages={:?}",
                    st.release, st.current_generation, st.lock, st.lock_packages
                ),
                Err(e) => log::info!("promin: status недоступен при refresh ({e})"),
            }
            // TODO(D-APPSTREAM): при refresh префетчить AppStream-каталог канала с
            // cache.svitlolinux.org (appstream::catalog_url) в local_catalog_path
            // ДО reload(). Раздача каталога сервером ещё не финализирована.
        }
        for c in self.appstream_caches.iter_mut() {
            c.reload();
        }
        // достраиваем промин-индекс: пакеты без компонента в каталоге кладём в кеш как
        // Addon (install-by-name бэкендом), НЕ как приложения — витрина показывает аппы,
        // не либы. ПОСЛЕ reload(): reload() парсит AppStream-каталог, добавляем недостающее
        self.augment_cache_with_recipes();
        Ok(())
    }

    fn info_caches(&self) -> &[AppstreamCache] {
        &self.appstream_caches
    }

    fn installed(&self) -> Result<Vec<Package>, Box<dyn Error>> {
        let list = self.installed_map()?;
        // версию берём из lock (точная), иначе из store_name basename (хвост)
        let mut out = Vec::new();
        for (name, store_name) in list.packages.iter() {
            let version = self
                .lock
                .as_ref()
                .and_then(|l| l.packages.get(name))
                .map(|e| e.version.clone())
                .unwrap_or_else(|| version_from_store_name(store_name, name));
            out.extend(self.packages_for(name, version));
        }
        Ok(out)
    }

    fn updates(&self) -> Result<Vec<Package>, Box<dyn Error>> {
        // updates через release-lock (PROMIN-BACKEND.md): пакет устарел если
        // store_name установленного != store_name в lock релиза/канала.
        // hash-pin меняется при любом изменении входов сборки -> точное сравнение.
        let Some(lock) = &self.lock else {
            // без lock (legacy recompute) надёжного diff нет — пусто
            log::info!("promin: lock отсутствует, updates() пуст (legacy recompute)");
            return Ok(Vec::new());
        };
        let installed = self.installed_map()?;
        let mut out = Vec::new();
        for (name, installed_store) in installed.packages.iter() {
            if let Some(entry) = lock.packages.get(name) {
                let locked_store = entry.store_name(name);
                if &locked_store != installed_store {
                    // обновление = смена поколения/канала (D-UPDATE). Карточка
                    // несёт новую (lock) версию + extra с установленной.
                    for mut p in self.packages_for(name, entry.version.clone()) {
                        p.extra.insert(
                            format!("{name}_installed"),
                            version_from_store_name(installed_store, name),
                        );
                        out.push(p);
                    }
                }
            }
        }
        Ok(out)
    }

    fn file_packages(&self, _path: &str) -> Result<Vec<Package>, Box<dyn Error>> {
        // promin не ставит из локального файла-пакета (binary-only с канала по
        // hash-pin). Локальный .tar.gz это bincache-восстановление вне UI-сценария.
        // ВЫВЕРИТЬ: если понадобится drag-n-drop binpkg — мапить на bincache import.
        Ok(Vec::new())
    }

    fn gstreamer_packages(
        &self,
        _gstreamer_codec: &GStreamerCodec,
    ) -> Result<Vec<Package>, Box<dyn Error>> {
        // codec-провайдинг promin не выражает (нет what-provides по media-type).
        // Кандидат на YAML-fallback metadata позже. Пока пусто.
        Ok(Vec::new())
    }

    fn operation(
        &self,
        op: &Operation,
        mut f: Box<dyn FnMut(f32) + 'static>,
    ) -> Result<(), Box<dyn Error>> {
        let names = Self::op_pkg_names(op);
        if names.is_empty() {
            return Err(format!("{:?}: нет имени промин-пакета", op.package_ids).into());
        }
        // client принимает ОДИН пакет за вызов (install <package>); ставим по очереди.
        // Прогресс грубый: client не стримит (см json_types::ProgressEvent / открытый
        // узел п.1). Делим шкалу по числу пакетов. ШКАЛА f() это 0..100 (потребитель
        // main.rs делит /100.0), НЕ 0..1 — выверено по вендоренному cosmic-store.
        // TODO(L3.1 эскалация): install/remove пишут в /promin/store + меняют
        //   поколение -> из GUI под юзером нужен pkexec-обёртка либо promin D-Bus
        //   сервис. Здесь зовём напрямую (работает под root/группой promin).
        let total = names.len() as f32;
        f(0.0);
        for (i, name) in names.iter().enumerate() {
            match &op.kind {
                OperationKind::Install | OperationKind::Update => {
                    // Update в нашей модели = переустановка по новому lock-пину.
                    let out = self.run_json(&["install", name])?;
                    let r = jt::parse_or_err::<jt::InstallResult>(&out).map_err(|e| -> Box<dyn Error> { e.into() })?;
                    if !r.ok {
                        return Err(r
                            .error
                            .unwrap_or_else(|| format!("install {name} не удался"))
                            .into());
                    }
                }
                OperationKind::Uninstall { .. } => {
                    let out = self.run_json(&["remove", name])?;
                    let r = jt::parse_or_err::<jt::RemoveResult>(&out).map_err(|e| -> Box<dyn Error> { e.into() })?;
                    if !r.ok {
                        return Err(r
                            .error
                            .unwrap_or_else(|| format!("remove {name} не удался"))
                            .into());
                    }
                }
                OperationKind::RepositoryAdd(_) | OperationKind::RepositoryRemove(_, _) => {
                    // каналы promin задаются config.json (servers/release), не
                    // per-repo операцией из UI. См D-UPDATE / C6 nonfree-канал.
                    return Err("promin: управление каналами через config.json, не операцией".into());
                }
            }
            f((i as f32 + 1.0) / total * 100.0);
        }
        f(100.0);
        Ok(())
    }
}

/// Чтение + serde-разбор JSON-файла (config.json/lock.json). None если нет/битый.
fn read_json<T: for<'de> serde::Deserialize<'de>>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<T>(&text) {
        Ok(v) => Some(v),
        Err(e) => {
            log::warn!("promin: {} не разобран: {e}", path.display());
            None
        }
    }
}

/// Вытащить версию из store_name basename вида `<hash>-<name>-<version>`.
/// Эвристика для случая без lock: берём хвост после `-<name>-`.
/// ВЫВЕРИТЬ формат store_name на реальном store (CLAUDE.md: <hash>-<name>-<version>).
fn version_from_store_name(store_name: &str, name: &str) -> String {
    let needle = format!("-{name}-");
    match store_name.find(&needle) {
        Some(i) => store_name[i + needle.len()..].to_string(),
        None => String::new(),
    }
}

// ---- лёгкая самопроверка маппинга JSON (без сети/процесса) ----
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_list() {
        let s = r#"{"packages":{"cosmic-files":"abc123-cosmic-files-1.0.0","wget":"def-wget-1.21"}}"#;
        let r = jt::parse_or_err::<jt::ListResult>(s).unwrap();
        assert_eq!(r.packages.len(), 2);
        assert_eq!(version_from_store_name(&r.packages["wget"], "wget"), "1.21");
    }

    #[test]
    fn parse_error_envelope() {
        let s = r#"{"ok":false,"error":"пакет 'foo' не в lock релиза 0.3"}"#;
        let e = jt::parse_or_err::<jt::InstallResult>(s);
        // parse_or_err ловит {ok:false} как ошибку
        assert!(e.is_err());
    }

    #[test]
    fn lock_store_name_reconstruct() {
        let e = jt::LockEntry {
            version: "1.0.0".into(),
            input_hash: "abc123".into(),
            runtime_deps: vec![],
            store_name: None,
        };
        assert_eq!(e.store_name("cosmic-files"), "abc123-cosmic-files-1.0.0");
    }
}

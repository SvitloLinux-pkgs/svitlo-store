// AppStream-каталог релиза (svitlo-store ProminBackend, L2.1)
//
// Метаданные приложений (имя/summary/описание/иконка/скриншоты/категории) +
// маппинг promin-пакет <-> AppStream-компонент.
//
// Источники (D-APPSTREAM, см PROMIN-BACKEND.md):
//   1) локальный кеш релиза, кладётся образом / синком с cache
//   2) cache.svitlolinux.org/releases/<ver>/catalog — фетч + кеш (ещё не раздаётся)
//
// СТРАТЕГИЯ: НЕ переписываем парсер AppStream — апстримный `AppstreamCache`
// (src/appstream_cache.rs) уже умеет грузить MetaInfo XML/YAML каталоги через
// reload() и хранит infos: HashMap<AppId, Arc<AppInfo>> + pkgnames-индекс.
// ProminBackend держит ОДИН AppstreamCache на промин-канал и переиспользует его
// icon()/infos/pkgnames ровно как packagekit.rs. Этот модуль = тонкая обвязка:
// путь к каталогу + конструктор + опциональный сетевой префетч.
//
// ВЫВЕРЕНО ПО ВЕНДОРЕННОМУ cosmic-store:
//   * AppstreamCache::new(source_id, source_name, paths: Vec<PathBuf>,
//     icons_paths: Vec<String>, locale) — публичный конструктор с ЯВНЫМИ путями.
//     Патч апстрима (with_paths) НЕ нужен: строим кеш на НАШ каталог напрямую.
//   * system() сканирует <prefix>/{swcatalog,app-info}/{xml,yaml}/* + .../icons.
//     Наш каталог зеркалит ту же раскладку: <catalog>/{xml,yaml}/*, <catalog>/icons.
//   * reload() (зовёт ProminBackend::load_caches) парсит файлы из path_tags,
//     заполняет infos/pkgnames. Нам остаётся дать верные пути в new().
//
// ВЫВЕРИТЬ В СРЕДЕ:
//   * формат раздачи каталога сервером (D-APPSTREAM ещё не финализирован)
//   * реальную раскладку каталога что кладёт образ (xml vs yaml, наличие icons)

use crate::AppstreamCache;
use std::path::{Path, PathBuf};

/// Корень локального кеша AppStream-каталога Svitlo на установленной системе.
/// Образ кладёт сюда каталог релиза; updates-синк обновляет.
pub const SVITLO_CATALOG_DIR: &str = "/var/lib/svitlo-store/appstream";

/// Идентификатор источника промин-канала (попадает в AppInfo.source_id и в UI).
pub const SOURCE_ID: &str = "promin";
pub const SOURCE_NAME: &str = "Svitlo";

/// URL раздачи каталога (D-APPSTREAM). releases/<release>/catalog по дизайну.
pub fn catalog_url(server: &str, release: &str) -> String {
    let server = server.trim_end_matches('/');
    format!("{server}/releases/{release}/catalog")
}

/// Локальный путь каталога релиза в кеше.
pub fn local_catalog_path(release: &str) -> PathBuf {
    Path::new(SVITLO_CATALOG_DIR).join(release)
}

/// Файлы каталога релиза для AppstreamCache::new (зеркалит layout system():
/// <catalog>/{xml,yaml}/* как отдельные пути). Возвращает (paths, icons_paths).
/// Если каталога нет — пустые векторы (кеш будет пуст, витрину достроит
/// ProminBackend::augment_cache_with_recipes из `--json recipes`).
fn catalog_entries(release: &str) -> (Vec<PathBuf>, Vec<String>) {
    let root = local_catalog_path(release);
    let mut paths = Vec::new();
    let mut icons = Vec::new();
    for fmt in &["xml", "yaml"] {
        let dir = root.join(fmt);
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                paths.push(e.path());
            }
        }
    }
    let icons_dir = root.join("icons");
    if icons_dir.is_dir() {
        icons.push(icons_dir.to_string_lossy().into_owned());
    }
    (paths, icons)
}

/// Строит AppstreamCache для промин-канала из НАШЕГО каталога релиза
/// (local_catalog_path(release)), НЕ системного swcatalog. Использует публичный
/// AppstreamCache::new с явными путями (выверено по вендоренному cosmic-store).
///
/// Если каталог релиза ещё не положен (D-APPSTREAM не раздаёт, образ без
/// каталога) — пути пусты, кеш пустой, и витрину/поиск даёт промин-индекс
/// (ProminBackend::augment_cache_with_recipes по `--json recipes`).
pub fn build_cache(locale: &str, release: &str) -> AppstreamCache {
    let (paths, icons_paths) = catalog_entries(release);
    AppstreamCache::new(
        SOURCE_ID.to_string(),
        SOURCE_NAME.to_string(),
        paths,
        icons_paths,
        locale,
    )
}

/// Маппинг promin-имя пакета -> AppId в каталоге.
///
/// promin оперирует slug-именами рецептов (напр "cosmic-files", "firefox-svitlo").
/// AppStream компонент несёт desktop-id (напр "com.system76.CosmicFiles") и
/// pkgname. Связь идёт через AppstreamCache.pkgnames: HashMap<String,HashSet<AppId>>
/// где ключ = pkgname рецепта. ProminBackend заполняет AppInfo.pkgnames именем
/// promin-пакета (как packagekit кладёт системное имя), поэтому lookup по
/// pkgname находит компонент.
///
/// Возвращает: список AppId каталога, относящихся к promin-пакету (обычно 0 или 1;
/// >1 для метапакетов). Пусто => метаданных нет, рисуем generic-карточку.
pub fn ids_for_pkg<'a>(cache: &'a AppstreamCache, promin_name: &str) -> Vec<&'a crate::AppId> {
    match cache.pkgnames.get(promin_name) {
        Some(ids) => ids.iter().collect(),
        None => Vec::new(),
    }
}

/// YAML-fallback метаданные для пакетов БЕЗ AppStream MetaInfo (CLI/шрифты/либы).
/// PROMIN-BACKEND.md «Открытые узлы» п.4: поле `metadata` рецепта (вне canonical).
/// Сервер агрегирует это в каталог; если нет — генерим минимальный AppInfo из
/// одного имени промина (см promin.rs::generic_info).
///
/// Здесь только тип под будущий YAML; парсинг — на стороне сервера каталога,
/// клиент потребляет уже единый AppStream. Оставлено как точка расширения.
#[derive(Debug, Clone, Default)]
pub struct YamlFallback {
    pub summary: String,
    pub categories: Vec<String>,
    pub icon_name: Option<String>,
}

// ВЫВЕРИТЬ: если решим парсить YAML-fallback на клиенте (а не агрегировать на
// сервере), сюда добавить serde-чтение `metadata`-блока рецепта и слияние в
// AppInfo до построения Package.

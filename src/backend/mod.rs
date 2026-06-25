// ДЕЛЬТА форка svitlo-store к upstream src/backend/mod.rs (ProminBackend, L1.1)
//
// ВАЖНО про стратегию форка (выверено по ВЕНДОРЕННОМУ cosmic-store):
//   Этот файл больше НЕ цельная замена апстримного mod.rs (стратегия b отвергнута).
//   Реальный upstream:
//     * `BackendName` это enum из 5 вариантов (FlatpakUser/FlatpakSystem/Packagekit/
//       Pkgar/RpmOstree) с as_str()/is_flatpak()/FromStr. Эти варианты используются
//       ПО ИМЕНИ вне mod.rs (src/priority.rs: BackendName::Packagekit/FlatpakUser),
//       поэтому их нельзя просто выкинуть — снести значит чинить ещё priority.rs.
//     * `backends(locale, refresh) -> impl Stream<Item=(BackendName, Arc<dyn Backend>)>`
//       (НЕ `backends(locale) -> Backends`). Зовётся из main.rs::update_backends как
//       поток с buffer_unordered(4) и load_caches на каждом бэкенде.
//   Поэтому дельта МИНИМАЛЬНАЯ и накладывается на апстримный mod.rs:
//     1) добавить вариант `BackendName::Promin` (+ ветки as_str/FromStr),
//     2) под нашим feature `promin` отключить чужие бэкенды и зарегистрировать один
//        ProminBackend в backends() с тем же Stream-контрактом,
//     3) feature `flatpak` НЕ включать в default (libflatpak в базе Svitlo нет),
//        бэкенды packagekit/pkgar/rpm-ostree и так выключены по feature.
//
// Этот файл держит подмодули дельты (promin/appstream/json_types) и СПРАВОЧНУЮ
// версию контракта (trait Backend, Package, BackendName, backends()), выверенную
// по вендоренному дереву. При вендоринге форка применяем дельту 1-2 поверх
// апстримного mod.rs (см plan/FORK-INTEGRATION.md), а не подменяем его целиком.
//
// ПОДМОДУЛИ ДЕЛЬТЫ:
//   promin      — ProminBackend impl Backend (subprocess promin --json)
//   appstream   — каталог релиза + маппинг pkg<->компонент
//   json_types  — serde под promin --json (выверено по client.py)

pub mod appstream;
pub mod json_types;
pub mod promin;

use cosmic::widget;
use std::{
    collections::HashMap,
    error::Error,
    fmt,
    sync::Arc,
};

use crate::{AppId, AppInfo, AppstreamCache, GStreamerCodec, Operation};

/// Имя бэкенда (UI группирует источники по нему). Дельта добавляет вариант Promin
/// к апстримному enum. Остальные варианты СОХРАНЯЮТСЯ (priority.rs ключует по ним
/// по имени; в Svitlo они просто не регистрируются — feature off).
///
/// Здесь справочно показан ПОЛНЫЙ enum после дельты. При накладывании на апстрим
/// добавляем только строку `Promin,` + ветки as_str/FromStr.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum BackendName {
    FlatpakUser,
    FlatpakSystem,
    Packagekit,
    Pkgar,
    RpmOstree,
    Promin,
}

impl BackendName {
    pub fn as_str(&self) -> &'static str {
        match self {
            BackendName::FlatpakUser => "flatpak-user",
            BackendName::FlatpakSystem => "flatpak-system",
            BackendName::Packagekit => "packagekit",
            BackendName::Pkgar => "pkgar",
            BackendName::RpmOstree => "rpm-ostree",
            BackendName::Promin => "promin",
        }
    }

    pub fn is_flatpak(&self) -> bool {
        matches!(self, BackendName::FlatpakUser | BackendName::FlatpakSystem)
    }
}

impl fmt::Display for BackendName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl std::str::FromStr for BackendName {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "flatpak-user" => Ok(BackendName::FlatpakUser),
            "flatpak-system" => Ok(BackendName::FlatpakSystem),
            "packagekit" => Ok(BackendName::Packagekit),
            "pkgar" => Ok(BackendName::Pkgar),
            "rpm-ostree" => Ok(BackendName::RpmOstree),
            "promin" => Ok(BackendName::Promin),
            _ => Err(format!("unknown backend name: {s}")),
        }
    }
}

/// Карточка пакета (контракт апстрима, поля дословно).
#[derive(Clone, Debug)]
pub struct Package {
    pub id: AppId,
    pub icon: widget::icon::Handle,
    pub info: Arc<AppInfo>,
    pub version: String,
    pub extra: HashMap<String, String>,
}

/// trait Backend — контракт апстрима cosmic-store (сигнатуры выверены по
/// вендоренному дереву). gstreamer_packages имеет дефолт у апстрима; мы метод
/// переопределяем явно (пусто), что совместимо.
pub trait Backend: fmt::Debug + Send + Sync {
    fn load_caches(&mut self, refresh: bool) -> Result<(), Box<dyn Error>>;
    fn info_caches(&self) -> &[AppstreamCache];
    fn installed(&self) -> Result<Vec<Package>, Box<dyn Error>>;
    fn updates(&self) -> Result<Vec<Package>, Box<dyn Error>>;
    fn file_packages(&self, path: &str) -> Result<Vec<Package>, Box<dyn Error>>;
    fn gstreamer_packages(
        &self,
        _gstreamer_codec: &GStreamerCodec,
    ) -> Result<Vec<Package>, Box<dyn Error>> {
        Ok(Vec::new())
    }
    fn operation(
        &self,
        op: &Operation,
        f: Box<dyn FnMut(f32) + 'static>,
    ) -> Result<(), Box<dyn Error>>;
}

/// Фабрика бэкендов. ДЕЛЬТА: апстрим стримит несколько (flatpak/packagekit/...) с
/// конкурентной загрузкой кешей и сигнатурой `backends(locale, refresh) -> impl
/// Stream<Item=(BackendName, Arc<dyn Backend>)>`. Мы держим ТОТ ЖЕ контракт
/// (main.rs::update_backends ждёт Stream), но конструируем один ProminBackend и
/// сразу грузим его кеши (как апстрим в map-стадии). Возврат — поток из одного
/// элемента.
///
/// ВЫВЕРИТЬ при бампе: если апстрим сменит сигнатуру/тип возврата backends()
/// (напр уберёт refresh либо тип Stream) — отразить здесь и в дельта-патче.
pub fn backends(
    locale: &str,
    refresh: bool,
) -> impl futures::Stream<Item = (BackendName, Arc<dyn Backend>)> + Send + Unpin + 'static {
    let item = match promin::ProminBackend::new(locale) {
        Ok(mut b) => {
            if let Err(e) = b.load_caches(refresh) {
                log::error!("promin: load_caches при инициализации не удался: {e}");
            }
            let backend: Arc<dyn Backend> = Arc::new(b);
            Some((BackendName::Promin, backend))
        }
        Err(e) => {
            log::error!("promin backend не инициализирован: {e}");
            None
        }
    };
    Box::pin(futures::stream::iter(item.into_iter()))
}

// type-алиас коллекции бэкендов (ваниль cosmic-store нёс его в backend/mod.rs;
// main.rs/search.rs импортируют `backend::Backends`)
pub type Backends = std::collections::BTreeMap<BackendName, std::sync::Arc<dyn Backend>>;

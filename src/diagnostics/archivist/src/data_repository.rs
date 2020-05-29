// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use {
    crate::{
        events::types::{ComponentIdentifier, InspectData},
        formatter,
    },
    anyhow::{format_err, Error},
    fidl::endpoints::DiscoverableService,
    fidl_fuchsia_diagnostics::{self, Selector},
    fidl_fuchsia_inspect::TreeMarker,
    fidl_fuchsia_inspect_deprecated::InspectMarker,
    fidl_fuchsia_io::{DirectoryProxy, NodeInfo, CLONE_FLAG_SAME_RIGHTS},
    files_async,
    fuchsia_async::{self as fasync, DurationExt, TimeoutExt},
    fuchsia_inspect::reader::snapshot::{Snapshot, SnapshotTree},
    fuchsia_inspect_node_hierarchy::{
        trie::{self, TrieIterableNode},
        InspectHierarchyMatcher, NodeHierarchy,
    },
    fuchsia_zircon::{self as zx, DurationNum},
    futures::future::BoxFuture,
    futures::stream::StreamExt,
    futures::{FutureExt, TryFutureExt},
    inspect_fidl_load as deprecated_inspect, io_util,
    log::error,
    parking_lot::Mutex,
    pin_utils::pin_mut,
    selectors,
    std::collections::HashMap,
    std::convert::{TryFrom, TryInto},
    std::path::{Path, PathBuf},
    std::sync::Arc,
};

// Number of seconds to wait before timing out various async operations;
//    1) Reading the diagnostics directory of a component, searching for inspect files.
//    2) Getting the description of a file proxy backing the inspect data.
//    3) Reading the bytes from a File.
//    4) Loading a hierachy from the deprecated inspect fidl protocol.
//    5) Converting an unpopulated data map into a populated data map.
pub static INSPECT_ASYNC_TIMEOUT_SECONDS: i64 = 5;

pub type InspectDataTrie = trie::Trie<String, UnpopulatedInspectDataContainer>;

pub enum ReadSnapshot {
    Single(Snapshot),
    Tree(SnapshotTree),
    Finished(NodeHierarchy),
}

/// Mapping from a diagnostics filename to the underlying encoding of that
/// diagnostics data.
pub type DataMap = HashMap<String, InspectData>;

pub type Moniker = String;

pub trait DataCollector {
    // Processes all previously collected data from the configured sources,
    // provides the returned DataMap with ownership of that data, returns the
    // map, and clears the collector state.
    //
    // If no data has yet been collected, or if the data had previously been
    // collected, then the return value will be None.
    fn take_data(self: Box<Self>) -> Option<DataMap>;

    // Triggers the process of collection, causing the collector to find and stage
    // all data it is configured to collect for transfer of ownership by the next
    // take_data call.
    fn collect(self: Box<Self>, path: PathBuf) -> BoxFuture<'static, Result<(), Error>>;
}

/// InspectDataCollector holds the information needed to retrieve the Inspect
/// VMOs associated with a particular component
#[derive(Clone, Debug)]
pub struct InspectDataCollector {
    /// The inspect data associated with a particular event.
    ///
    /// This is wrapped in an Arc Mutex so it can be shared between multiple data sources.
    ///
    /// Note: The Arc is needed so that we can both add the data map to a data collector
    ///       and trigger async collection of the data in the same method. This can only
    ///       be done by allowing the async method to populate the same data that is being
    ///       passed into the component event.
    inspect_data_map: Arc<Mutex<Option<DataMap>>>,
}

impl InspectDataCollector {
    /// Construct a new InspectDataCollector, wrapped by an Arc<Mutex>.
    pub fn new() -> Self {
        InspectDataCollector { inspect_data_map: Arc::new(Mutex::new(Some(DataMap::new()))) }
    }

    /// Convert a fully-qualified path to a directory-proxy in the executing namespace.
    /// NOTE: Currently does a synchronous directory-open, since there are no available
    ///       async apis.
    pub async fn find_directory_proxy(path: &Path) -> Result<DirectoryProxy, Error> {
        // TODO(36762): When available, use the async directory-open api.
        return io_util::open_directory_in_namespace(
            &path.to_string_lossy(),
            io_util::OPEN_RIGHT_READABLE | io_util::OPEN_RIGHT_WRITABLE,
        );
    }

    /// Searches the directory specified by inspect_directory_proxy for
    /// .inspect files and populates the `inspect_data_map` with the found VMOs.
    pub async fn populate_data_map(&mut self, inspect_proxy: &DirectoryProxy) -> Result<(), Error> {
        // TODO(36762): Use a streaming and bounded readdir API when available to avoid
        // being hung.
        let entries = files_async::readdir_recursive(
            inspect_proxy,
            Some(INSPECT_ASYNC_TIMEOUT_SECONDS.seconds()),
        )
        .filter_map(|result| {
            async move {
                // TODO(fxb/49157): decide how to show directories that we failed to read.
                result.ok()
            }
        });
        pin_mut!(entries);
        while let Some(entry) = entries.next().await {
            // We are only currently interested in inspect VMO files (root.inspect) and
            // inspect services.
            if let Ok(Some(proxy)) = self.maybe_load_service::<TreeMarker>(inspect_proxy, &entry) {
                let maybe_vmo = proxy
                    .get_content()
                    .err_into::<anyhow::Error>()
                    .on_timeout(INSPECT_ASYNC_TIMEOUT_SECONDS.seconds().after_now(), || {
                        Err(format_err!("Timed out reading contents via Tree protocol."))
                    })
                    .await?
                    .buffer
                    .map(|b| b.vmo);

                self.maybe_add(&entry.name, InspectData::Tree(proxy, maybe_vmo));
                continue;
            }

            if let Ok(Some(proxy)) = self.maybe_load_service::<InspectMarker>(inspect_proxy, &entry)
            {
                self.maybe_add(&entry.name, InspectData::DeprecatedFidl(proxy));
                continue;
            }

            if !entry.name.ends_with(".inspect") || entry.kind != files_async::DirentKind::File {
                continue;
            }

            let file_proxy = match io_util::open_file(
                inspect_proxy,
                Path::new(&entry.name),
                io_util::OPEN_RIGHT_READABLE,
            ) {
                Ok(proxy) => proxy,
                Err(_) => {
                    continue;
                }
            };

            // Obtain the vmo backing any VmoFiles.
            match file_proxy
                .describe()
                .err_into::<anyhow::Error>()
                .on_timeout(INSPECT_ASYNC_TIMEOUT_SECONDS.seconds().after_now(), || {
                    Err(format_err!(
                        "Timed out waiting for backing file description: {:?}",
                        file_proxy
                    ))
                })
                .await
            {
                Ok(nodeinfo) => match nodeinfo {
                    NodeInfo::Vmofile(vmofile) => {
                        self.maybe_add(&entry.name, InspectData::Vmo(vmofile.vmo));
                    }
                    NodeInfo::File(_) => {
                        let contents = io_util::read_file_bytes(&file_proxy)
                            .on_timeout(INSPECT_ASYNC_TIMEOUT_SECONDS.seconds().after_now(), || {
                                Err(format_err!(
                                    "Timed out reading contents of fuchsia File: {:?}",
                                    file_proxy
                                ))
                            })
                            .await?;
                        self.maybe_add(&entry.name, InspectData::File(contents));
                    }
                    ty @ _ => {
                        error!(
                            "found an inspect file '{}' of unexpected type {:?}",
                            &entry.name, ty
                        );
                    }
                },
                Err(_) => {}
            }
        }

        Ok(())
    }

    /// Adds a key value to the contained vector if it hasn't been taken yet. Otherwise, does
    /// nothing.
    fn maybe_add(&mut self, key: impl Into<String>, value: InspectData) {
        if let Some(map) = self.inspect_data_map.lock().as_mut() {
            map.insert(key.into(), value);
        };
    }

    fn maybe_load_service<S: DiscoverableService>(
        &self,
        dir_proxy: &DirectoryProxy,
        entry: &files_async::DirEntry,
    ) -> Result<Option<S::Proxy>, Error> {
        if entry.name.ends_with(S::SERVICE_NAME) {
            let (proxy, server) = fidl::endpoints::create_proxy::<S>()?;
            fdio::service_connect_at(dir_proxy.as_ref(), &entry.name, server.into_channel())?;
            return Ok(Some(proxy));
        }
        Ok(None)
    }
}

impl DataCollector for InspectDataCollector {
    /// Takes the contained extra data. Additions following this have no effect.
    fn take_data(self: Box<Self>) -> Option<DataMap> {
        self.inspect_data_map.lock().take()
    }

    /// Collect extra data stored under the given path.
    ///
    /// This currently only does a single pass over the directory to find information.
    fn collect(mut self: Box<Self>, path: PathBuf) -> BoxFuture<'static, Result<(), Error>> {
        async move {
            let inspect_proxy = match InspectDataCollector::find_directory_proxy(&path)
                .on_timeout(INSPECT_ASYNC_TIMEOUT_SECONDS.seconds().after_now(), || {
                    Err(format_err!("Timed out converting path into directory proxy: {:?}", path))
                })
                .await
            {
                Ok(proxy) => proxy,
                Err(e) => {
                    return Err(format_err!("Failed to open out directory at {:?}: {}", path, e));
                }
            };

            self.populate_data_map(&inspect_proxy).await
        }
        .boxed()
    }
}

/// Packet containing a snapshot and all the metadata needed to
/// populate a diagnostics schema for that snapshot.
pub struct SnapshotData {
    // Name of the file that created this snapshot.
    pub filename: String,
    // Timestamp at which this snapshot resolved or failed.
    pub timestamp: zx::Time,
    // Errors encountered when processing this snapshot.
    pub errors: Vec<formatter::Error>,
    // Optional snapshot of the inspect hierarchy, in case reading fails
    // and we have errors to share with client.
    pub snapshot: Option<ReadSnapshot>,
}

impl SnapshotData {
    // Constructs packet that timestamps and packages inspect snapshot for exfiltration.
    fn successful(snapshot: ReadSnapshot, filename: String) -> SnapshotData {
        SnapshotData {
            filename,
            timestamp: fasync::Time::now().into_zx(),
            errors: Vec::new(),
            snapshot: Some(snapshot),
        }
    }

    // Constructs packet that timestamps and packages inspect snapshot failure for exfiltration.
    fn failed(error: formatter::Error, filename: String) -> SnapshotData {
        SnapshotData {
            filename,
            timestamp: fasync::Time::now().into_zx(),
            errors: vec![error],
            snapshot: None,
        }
    }
}

/// PopulatedInspectDataContainer is the container that
/// holds the actual Inspect data for a given component,
/// along with all information needed to transform that data
/// to be returned to the client.
pub struct PopulatedInspectDataContainer {
    /// Relative moniker of the component that this populated
    /// data packet has gathered data for.
    pub relative_moniker: Vec<String>,
    /// Vector of all the snapshots of inspect hierarchies under
    /// the diagnostics directory of the component identified by
    /// relative_moniker, along with the metadata needed to populate
    /// this snapshot's diagnostics schema.
    pub snapshots: Vec<SnapshotData>,
    /// Optional hierarchy matcher. If unset, the reader is running
    /// in all-access mode, meaning no matching or filtering is required.
    pub inspect_matcher: Option<InspectHierarchyMatcher>,
}

impl PopulatedInspectDataContainer {
    pub async fn try_from(
        unpopulated: UnpopulatedInspectDataContainer,
    ) -> Result<PopulatedInspectDataContainer, Error> {
        let mut collector = InspectDataCollector::new();

        match collector.populate_data_map(&unpopulated.component_diagnostics_proxy).await {
            Ok(_) => {
                let mut snapshots_data_opt = None;
                if let Some(data_map) = Box::new(collector).take_data() {
                    let mut acc: Vec<SnapshotData> = vec![];
                    for (filename, data) in data_map {
                        match data {
                            InspectData::Tree(tree, _) => match SnapshotTree::try_from(&tree).await {
                                Ok(snapshot_tree) => {
                                    acc.push(SnapshotData::successful(
                                        ReadSnapshot::Tree(snapshot_tree), filename));
                                }
                                Err(e) => {
                                    acc.push(SnapshotData::failed(
                                        formatter::Error{message: format!("{:?}", e)}, filename));
                                }
                            },
                            InspectData::DeprecatedFidl(inspect_proxy) => {
                                match deprecated_inspect::load_hierarchy(inspect_proxy)
                                    .on_timeout(
                                        INSPECT_ASYNC_TIMEOUT_SECONDS.seconds().after_now(),
                                        || {
                                            Err(format_err!(
                                                "Timed out reading via deprecated inspect protocol.",
                                            ))
                                        },
                                    )
                                    .await
                                {
                                    Ok(hierarchy) => {
                                        acc.push(SnapshotData::successful(
                                            ReadSnapshot::Finished(hierarchy), filename));
                                    }
                                    Err(e) => {
                                        acc.push(SnapshotData::failed(
                                            formatter::Error{message: format!("{:?}", e)}, filename));
                                   }
                                }
                            }
                            InspectData::Vmo(vmo) => match Snapshot::try_from(&vmo) {
                                Ok(snapshot) => {
                                    acc.push(SnapshotData::successful(
                                        ReadSnapshot::Single(snapshot), filename));
                                }
                                Err(e) => {
                                    acc.push(SnapshotData::failed(
                                        formatter::Error{message: format!("{:?}", e)}, filename));
                                }
                            },
                            InspectData::File(contents) => match Snapshot::try_from(contents) {
                                Ok(snapshot) => {
                                    acc.push(SnapshotData::successful(ReadSnapshot::Single(snapshot), filename));
                                }
                                Err(e) => {
                                    acc.push(SnapshotData::failed(
                                        formatter::Error{message: format!("{:?}", e)}, filename));
                                }
                            },
                            InspectData::Empty => {}
                        }
                    }
                    snapshots_data_opt = Some(acc);
                }
                match snapshots_data_opt {
                    Some(snapshots) => Ok(PopulatedInspectDataContainer {
                        relative_moniker: unpopulated.relative_moniker,
                        snapshots: snapshots,
                        inspect_matcher: unpopulated.inspect_matcher,
                    }),
                    None => Err(format_err!(
                        "Failed to parse snapshots for: {:?}.",
                        unpopulated.relative_moniker
                    )),
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// UnpopulatedInspectDataContainer is the container that holds
/// all information needed to retrieve Inspect data
/// for a given component, when requested.
pub struct UnpopulatedInspectDataContainer {
    /// Relative moniker of the component that this data container
    /// is representing.
    pub relative_moniker: Vec<String>,
    /// DirectoryProxy for the out directory that this
    /// data packet is configured for.
    pub component_diagnostics_proxy: DirectoryProxy,
    /// Optional hierarchy matcher. If unset, the reader is running
    /// in all-access mode, meaning no matching or filtering is required.
    pub inspect_matcher: Option<InspectHierarchyMatcher>,
}

/// InspectDataRepository manages storage of all state needed in order
/// for the inspect reader to retrieve inspect data when a read is requested.
pub struct InspectDataRepository {
    pub data_directories: InspectDataTrie,
    /// Optional static selectors. For the all_access reader, there
    /// are no provided selectors. For all other pipelines, a non-empty
    /// vector is required.
    pub static_selectors: Option<Vec<Arc<Selector>>>,
}

impl InspectDataRepository {
    pub fn new(static_selectors: Option<Vec<Arc<Selector>>>) -> Self {
        InspectDataRepository {
            data_directories: InspectDataTrie::new(),
            static_selectors: static_selectors,
        }
    }

    pub fn remove(&mut self, component_id: &ComponentIdentifier) {
        self.data_directories.remove(component_id.unique_key());
    }

    pub fn add(
        &mut self,
        identifier: ComponentIdentifier,
        directory_proxy: DirectoryProxy,
    ) -> Result<(), Error> {
        let relative_moniker = identifier.relative_moniker_for_selectors();
        let matched_selectors = match &self.static_selectors {
            Some(selectors) => Some(selectors::match_component_moniker_against_selectors(
                &relative_moniker,
                &selectors,
            )?),
            None => None,
        };

        // The component events stream might contain duplicated events for out/diagnostics
        // directories of components that already existed before the archivist started or the
        // archivist itself, make sure we don't track duplicated component diagnostics directories.
        if self.contains(&identifier, &relative_moniker) {
            return Ok(());
        }

        let key = identifier.unique_key();
        match matched_selectors {
            Some(selectors) => {
                if !selectors.is_empty() {
                    self.data_directories.insert(
                        key,
                        UnpopulatedInspectDataContainer {
                            relative_moniker: relative_moniker,
                            component_diagnostics_proxy: directory_proxy,
                            inspect_matcher: Some((&selectors).try_into()?),
                        },
                    );
                }
                Ok(())
            }
            None => {
                self.data_directories.insert(
                    key,
                    UnpopulatedInspectDataContainer {
                        relative_moniker: relative_moniker,
                        component_diagnostics_proxy: directory_proxy,
                        inspect_matcher: None,
                    },
                );

                Ok(())
            }
        }
    }

    /// Return all of the DirectoryProxies that contain Inspect hierarchies
    /// which contain data that should be selected from.
    pub fn fetch_data(&self) -> Vec<UnpopulatedInspectDataContainer> {
        return self
            .data_directories
            .iter()
            .filter_map(
                |(_, unpopulated_data_container_opt)| match unpopulated_data_container_opt {
                    Some(unpopulated_data_container) => io_util::clone_directory(
                        &unpopulated_data_container.component_diagnostics_proxy,
                        CLONE_FLAG_SAME_RIGHTS,
                    )
                    .ok()
                    .map(|directory| UnpopulatedInspectDataContainer {
                        relative_moniker: unpopulated_data_container.relative_moniker.clone(),
                        component_diagnostics_proxy: directory,
                        inspect_matcher: unpopulated_data_container.inspect_matcher.clone(),
                    }),
                    None => None,
                },
            )
            .collect();
    }

    fn contains(
        &mut self,
        component_id: &ComponentIdentifier,
        relative_moniker: &[String],
    ) -> bool {
        self.data_directories
            .get(component_id.unique_key())
            .map(|trie_node| {
                trie_node
                    .get_values()
                    .iter()
                    .any(|container| container.relative_moniker == relative_moniker)
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        crate::{
            data_repository::{DataCollector, InspectDataCollector},
            events::types::{ComponentIdentifier, InspectData, LegacyIdentifier, RealmPath},
        },
        fdio,
        fidl::endpoints::DiscoverableService,
        fidl_fuchsia_inspect::TreeMarker,
        fidl_fuchsia_io::DirectoryMarker,
        fuchsia_async as fasync,
        fuchsia_component::server::ServiceFs,
        fuchsia_inspect::reader::PartialNodeHierarchy,
        fuchsia_inspect::{assert_inspect_tree, reader, Inspector},
        fuchsia_inspect_node_hierarchy::{trie::TrieIterableNode, NodeHierarchy},
        fuchsia_zircon as zx,
        fuchsia_zircon::Peered,
        futures::{FutureExt, StreamExt},
        std::path::PathBuf,
    };

    fn get_vmo(text: &[u8]) -> zx::Vmo {
        let vmo = zx::Vmo::create(4096).unwrap();
        vmo.write(text, 0).unwrap();
        vmo
    }

    #[fasync::run_singlethreaded(test)]
    async fn inspect_data_collector() {
        let path = PathBuf::from("/test-bindings");
        // Make a ServiceFs containing two files.
        // One is an inspect file, and one is not.
        let mut fs = ServiceFs::new();
        let vmo = get_vmo(b"test1");
        let vmo2 = get_vmo(b"test2");
        let vmo3 = get_vmo(b"test3");
        let vmo4 = get_vmo(b"test4");
        fs.dir("diagnostics").add_vmo_file_at("root.inspect", vmo, 0, 4096);
        fs.dir("diagnostics").add_vmo_file_at("root_not_inspect", vmo2, 0, 4096);
        fs.dir("diagnostics").dir("a").add_vmo_file_at("root.inspect", vmo3, 0, 4096);
        fs.dir("diagnostics").dir("b").add_vmo_file_at("root.inspect", vmo4, 0, 4096);
        // Create a connection to the ServiceFs.
        let (h0, h1) = zx::Channel::create().unwrap();
        fs.serve_connection(h1).unwrap();

        let ns = fdio::Namespace::installed().unwrap();
        ns.bind(path.join("out").to_str().unwrap(), h0).unwrap();

        fasync::spawn(fs.collect());

        let (done0, done1) = zx::Channel::create().unwrap();

        let thread_path = path.join("out/diagnostics");

        // Run the actual test in a separate thread so that it does not block on FS operations.
        // Use signalling on a zx::Channel to indicate that the test is done.
        std::thread::spawn(move || {
            let path = thread_path;
            let done = done1;
            let mut executor = fasync::Executor::new().unwrap();

            executor.run_singlethreaded(async {
                let collector = InspectDataCollector::new();
                // Trigger collection on a clone of the inspect collector so
                // we can use collector to take the collected data.
                Box::new(collector.clone()).collect(path).await.unwrap();
                let collector: Box<InspectDataCollector> = Box::new(collector);

                let extra_data = collector.take_data().expect("collector missing data");
                assert_eq!(3, extra_data.len());

                let assert_extra_data = |path: &str, content: &[u8]| {
                    let extra = extra_data.get(path);
                    assert!(extra.is_some());

                    match extra.unwrap() {
                        InspectData::Vmo(vmo) => {
                            let mut buf = [0u8; 5];
                            vmo.read(&mut buf, 0).expect("reading vmo");
                            assert_eq!(content, &buf);
                        }
                        v => {
                            panic!("Expected Vmo, got {:?}", v);
                        }
                    }
                };

                assert_extra_data("root.inspect", b"test1");
                assert_extra_data("a/root.inspect", b"test3");
                assert_extra_data("b/root.inspect", b"test4");

                done.signal_peer(zx::Signals::NONE, zx::Signals::USER_0).expect("signalling peer");
            });
        });

        fasync::OnSignals::new(&done0, zx::Signals::USER_0).await.unwrap();
        ns.unbind(path.join("out").to_str().unwrap()).unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn inspect_data_collector_tree() {
        let path = PathBuf::from("/test-bindings2");

        // Make a ServiceFs serving an inspect tree.
        let mut fs = ServiceFs::new();
        let inspector = Inspector::new();
        inspector.root().record_int("a", 1);
        inspector.root().record_lazy_child("lazy", || {
            async move {
                let inspector = Inspector::new();
                inspector.root().record_double("b", 3.14);
                Ok(inspector)
            }
            .boxed()
        });
        inspector.serve(&mut fs).expect("failed to serve inspector");

        // Create a connection to the ServiceFs.
        let (h0, h1) = zx::Channel::create().unwrap();
        fs.serve_connection(h1).unwrap();

        let ns = fdio::Namespace::installed().unwrap();
        ns.bind(path.join("out").to_str().unwrap(), h0).unwrap();

        fasync::spawn(fs.collect());

        let (done0, done1) = zx::Channel::create().unwrap();
        let thread_path = path.join("out/diagnostics");

        // Run the actual test in a separate thread so that it does not block on FS operations.
        // Use signalling on a zx::Channel to indicate that the test is done.
        std::thread::spawn(move || {
            let path = thread_path;
            let done = done1;
            let mut executor = fasync::Executor::new().unwrap();

            executor.run_singlethreaded(async {
                let collector = InspectDataCollector::new();

                //// Trigger collection on a clone of the inspect collector so
                //// we can use collector to take the collected data.
                Box::new(collector.clone()).collect(path).await.unwrap();
                let collector: Box<InspectDataCollector> = Box::new(collector);

                let extra_data = collector.take_data().expect("collector missing data");
                assert_eq!(1, extra_data.len());

                let extra = extra_data.get(TreeMarker::SERVICE_NAME);
                assert!(extra.is_some());

                match extra.unwrap() {
                    InspectData::Tree(tree, vmo) => {
                        // Assert we can read the tree proxy and get the data we expected.
                        let hierarchy = reader::read_from_tree(&tree)
                            .await
                            .expect("failed to read hierarchy from tree");
                        assert_inspect_tree!(hierarchy, root: {
                            a: 1i64,
                            lazy: {
                                b: 3.14,
                            }
                        });
                        let partial_hierarchy: NodeHierarchy =
                            PartialNodeHierarchy::try_from(vmo.as_ref().unwrap())
                                .expect("failed to read hierarchy from vmo")
                                .into();
                        // Assert the vmo also points to that data (in this case since there's no
                        // lazy nodes).
                        assert_inspect_tree!(partial_hierarchy, root: {
                            a: 1i64,
                        });
                    }
                    v => {
                        panic!("Expected Tree, got {:?}", v);
                    }
                }

                done.signal_peer(zx::Signals::NONE, zx::Signals::USER_0).expect("signalling peer");
            });
        });

        fasync::OnSignals::new(&done0, zx::Signals::USER_0).await.unwrap();
        ns.unbind(path.join("out").to_str().unwrap()).unwrap();
    }

    #[fasync::run_singlethreaded(test)]
    async fn inspect_repo_disallows_duplicated_dirs() {
        let mut inspect_repo = InspectDataRepository::new(None);
        let realm_path = RealmPath(vec!["a".to_string(), "b".to_string()]);
        let instance_id = "1234".to_string();

        let component_id = ComponentIdentifier::Legacy(LegacyIdentifier {
            instance_id,
            realm_path,
            component_name: "foo.cmx".into(),
        });
        let (proxy, _) =
            fidl::endpoints::create_proxy::<DirectoryMarker>().expect("create directory proxy");
        inspect_repo.add(component_id.clone(), proxy).expect("add to repo");

        let (proxy, _) =
            fidl::endpoints::create_proxy::<DirectoryMarker>().expect("create directory proxy");
        inspect_repo.add(component_id.clone(), proxy).expect("add to repo");

        let key = component_id.unique_key();
        assert_eq!(inspect_repo.data_directories.get(key).unwrap().get_values().len(), 1);
    }
}
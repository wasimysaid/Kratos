// App session root: secure pairing, embedded Tailcat connectivity, workspace
// connection, and the per-chat session-store cache. Demo data stays offline.

import Foundation
import Network
import Observation
import SwiftUI
import UIKit
import os

struct AuthTransitionGate {
    private(set) var generation = 0

    mutating func begin() -> Int {
        generation += 1
        return generation
    }

    mutating func cancel() { generation += 1 }
    func accepts(_ attempt: Int) -> Bool { attempt == generation }
}

private final class BackgroundFlushState: @unchecked Sendable {
    private let lock = NSLock()
    private var identifier: UIBackgroundTaskIdentifier = .invalid
    private var cancelled = false

    func setIdentifier(_ identifier: UIBackgroundTaskIdentifier) {
        lock.lock()
        self.identifier = identifier
        lock.unlock()
    }

    func cancel() -> UIBackgroundTaskIdentifier {
        lock.lock()
        cancelled = true
        let identifier = self.identifier
        self.identifier = .invalid
        lock.unlock()
        return identifier
    }

    func finish() -> UIBackgroundTaskIdentifier {
        lock.lock()
        let identifier = self.identifier
        self.identifier = .invalid
        lock.unlock()
        return identifier
    }

    var isCancelled: Bool {
        lock.lock()
        let cancelled = self.cancelled
        lock.unlock()
        return cancelled
    }
}
@MainActor
@Observable
final class AppModel {
    enum Phase { case signedOut, ready }

    var phase: Phase = .signedOut
    var workspace: WorkspaceStore?
    var demo: DemoDataset?
    var readinessError: String?
    var authBusy = false
    var demoPinnedSessionIds: [String] = []
    /// Graced connectivity truth shared by all status consumers.
    let connectivity = ConnectivityCenter()
    private var sessionStores: [String: SessionStore] = [:]
    @ObservationIgnored private var storeLastUsed: [String: UInt64] = [:]
    @ObservationIgnored private var usageClock: UInt64 = 0
    @ObservationIgnored private var memoryWarningObserver: NSObjectProtocol?
    private var config: AppConfig?
    @ObservationIgnored private var tailcat: (any TailcatClient)?
    @ObservationIgnored private var pathMonitor: NWPathMonitor?
    @ObservationIgnored private var lastPathKey: String?
    @ObservationIgnored private var restored = false

    @ObservationIgnored private var authGate = AuthTransitionGate()

    typealias NativeFactory = @Sendable (String, URL, String?) throws -> any TailcatClient
    typealias SessionRenewer = (URL, DeviceIdentity) async throws -> PairingSession
    typealias InvitationRedeemer = (URL, PairingInvitation, DeviceIdentity, String) async throws -> Void
    typealias IdentityLoader = (String) -> DeviceIdentity?
    @ObservationIgnored private let nativeFactory: NativeFactory
    @ObservationIgnored private let sessionRenewer: SessionRenewer
    @ObservationIgnored private let invitationRedeemer: InvitationRedeemer
    @ObservationIgnored private let identityLoader: IdentityLoader

    init(nativeFactory: @escaping NativeFactory = { address, directory, derpMap in
             try NativeTailcatClient(address: address, stateDirectory: directory, derpMap: derpMap)
         },
         sessionRenewer: @escaping SessionRenewer = { url, identity in
             try await AuthClient(baseURL: url).renew(identity: identity)
         },
         invitationRedeemer: @escaping InvitationRedeemer = { url, invitation, identity, deviceName in
             try await AuthClient(baseURL: url).redeem(invitation: invitation, identity: identity,
                                                        deviceName: deviceName)
         },
         identityLoader: @escaping IdentityLoader = { DeviceIdentity.load(profileId: $0) }) {
        self.nativeFactory = nativeFactory
        self.sessionRenewer = sessionRenewer
        self.invitationRedeemer = invitationRedeemer
        self.identityLoader = identityLoader
        memoryWarningObserver = NotificationCenter.default.addObserver(
            forName: UIApplication.didReceiveMemoryWarningNotification,
            object: nil,
            queue: .main
        ) { [weak self] _ in
            Task { @MainActor [weak self] in
                self?.evictForMemoryWarning()
            }
        }
    }

    @ObservationIgnored @AppStorage("pairedProfileId") var storedProfileId = ""
    @ObservationIgnored @AppStorage("pairedPeerAddress") var peerAddressString = ""
    @ObservationIgnored @AppStorage("pairedDERPMap") var storedDERPMap = ""
    @ObservationIgnored @AppStorage("deviceId") var storedDeviceId = ""


    deinit {
        if let memoryWarningObserver {
            NotificationCenter.default.removeObserver(memoryWarningObserver)
        }
    }

    var deviceId: String {
        if storedDeviceId.isEmpty {
            storedDeviceId = "ios-" + UUID().uuidString.lowercased().prefix(8)
        }
        return storedDeviceId
    }

    var deviceName: String { UIDevice.current.name }

    var launchRoute: Route?
    var launchSheet: String?
    var launchAutosend = false
    var launchFocusComposer = false

    func restore() {
        guard !restored, demo == nil else { return }
        restored = true
        let args = ProcessInfo.processInfo.arguments
        if args.contains("-bench") {
            Task { await BenchRunner.run() }
            return
        }
        if args.contains("-e2e") {
            Task { await E2ERunner.run(model: self) }
            return
        }
        if args.contains("-demo") {
            enterDemoMode()
            configureDemo(arguments: args)
            return
        }
        startPathMonitor()
        guard !storedProfileId.isEmpty, !peerAddressString.isEmpty,
              let identity = identityLoader(storedProfileId) else { return }
        // Project the profile cache immediately; network authentication upgrades
        // these same stores when available without forcing a new invitation.
        DocDisk.activate(profileId: identity.profileId)
        let offline = AppConfig(peerURL: URL(string: "http://127.0.0.1:1")!,
                                profileId: identity.profileId, deviceId: identity.deviceId,
                                deviceName: deviceName, bearer: "")
        config = offline
        workspace = WorkspaceStore(config: offline)
        phase = .ready
        retrySavedIdentity()
    }

    private func configureDemo(arguments args: [String]) {

        if let ix = args.firstIndex(of: "-sethomefilter"), ix + 1 < args.count {
            UserDefaults.standard.set(args[ix + 1], forKey: "homeSpaceFilter")
        }
        if args.contains("-no-projects") {
            demo?.spaces = []
            demo?.chats = []
            demo?.sessions = [:]
        }
        if args.contains("-ios-only") {
            demo?.devices = [DeviceRow(id: "ios-demo", name: "iPhone", platform: "ios")]
        }
        if let ix = args.firstIndex(of: "-route"), ix + 1 < args.count {
            let spec = args[ix + 1]
            if spec.hasPrefix("chat:") {
                let chatId = String(spec.dropFirst("chat:".count))
                launchRoute = .chat(chatId)
                if args.contains("-big") { demo?.sessionStore(for: chatId)
                    .setEntries(BenchRunner.syntheticEntries(turns: 120)) }
                if args.contains("-huge") { demo?.sessionStore(for: chatId)
                    .setEntries(BenchRunner.syntheticEntries(turns: 600)) }
                if args.contains("-stream"), let store = demo?.sessionStore(for: chatId) {
                    Task { @MainActor in
                        try? await Task.sleep(nanoseconds: 2_000_000_000)
                        store.demoResponder?("Show me the streamed reply path.")
                    }
                }
            } else if spec.hasPrefix("space:") {
                launchRoute = .space(String(spec.dropFirst("space:".count)))
            }
        }
        if let ix = args.firstIndex(of: "-sheet"), ix + 1 < args.count {
            launchSheet = args[ix + 1]
        }
        launchAutosend = args.contains("-autosend")
        launchFocusComposer = args.contains("-focuscomposer")
        if args.contains("-hydrate-late"), case .chat(let id)? = launchRoute,
           let store = demo?.sessionStore(for: id) {
            let full = store.entries
            store.setEntries([])
            Task { @MainActor in
                try? await Task.sleep(nanoseconds: 2_500_000_000)
                store.setEntries(full)
            }
        }
        func scheduledToggle(_ flag: String, archived: Bool) {
            guard let ix = args.firstIndex(of: flag), ix + 1 < args.count else { return }
            let parts = args[ix + 1].split(separator: ":")
            guard parts.count == 2, let seconds = Double(parts[1]) else { return }
            let chatId = String(parts[0])
            Task { @MainActor in
                try? await Task.sleep(nanoseconds: UInt64(seconds * 1_000_000_000))
                withAnimation(Motion.resort) {
                    if archived { self.archive(chatId: chatId) }
                    else { self.unarchive(chatId: chatId) }
                }
            }
        }
        scheduledToggle("-archive-after", archived: true)
        scheduledToggle("-unarchive-after", archived: false)
    }

    private func restorePaired(native: any TailcatClient, identity: DeviceIdentity,
                               generation: Int) async {
        do {
            guard authGate.accepts(generation), demo == nil else { native.close(); return }
            let session = try await sessionRenewer(native.url, identity)
            guard authGate.accepts(generation), demo == nil else { native.close(); return }
            connect(native: native, identity: identity, session: session, generation: generation)
            authBusy = false
            readinessError = nil
        } catch PairingError.revoked {
            native.close()
            guard authGate.accepts(generation) else { return }
            authBusy = false
            handleRevoked(profileId: identity.profileId, deviceId: identity.deviceId,
                          generation: generation)
        } catch {
            native.close()
            guard authGate.accepts(generation) else { return }
            authBusy = false
            readinessError = error.localizedDescription
        }
    }

    private func restorePairedFailed(identity: DeviceIdentity, generation: Int,
                                     revoked: Bool, message: String? = nil) {
        guard authGate.accepts(generation) else { return }
        authBusy = false
        if revoked {
            handleRevoked(profileId: identity.profileId, deviceId: identity.deviceId,
                          generation: generation)
        } else {
            readinessError = message
        }
    }

    func retrySavedIdentity() {
        guard !storedProfileId.isEmpty, !peerAddressString.isEmpty, !authBusy,
              let identity = identityLoader(storedProfileId) else { return }
        let generation = authGate.begin()
        let address = peerAddressString
        let derpMap = storedDERPMap.isEmpty ? nil : storedDERPMap
        let directory = tailcatDirectory(profileId: identity.profileId)
        let factory = nativeFactory
        authBusy = true
        // Launch the blocking Go client constructor directly from this call.
        // Routing through a MainActor Task first lets foreground network work
        // delay startup before the detached factory is even scheduled.
        Task.detached(priority: .userInitiated) { [weak self] in
            do {
                try Task.checkCancellation()
                let native = try factory(address, directory, derpMap)
                if Task.isCancelled {
                    native.close()
                    throw CancellationError()
                }
                guard let self else { native.close(); return }
                await self.restorePaired(native: native, identity: identity,
                                         generation: generation)
            } catch PairingError.revoked {
                await self?.restorePairedFailed(identity: identity, generation: generation,
                                                revoked: true)
            } catch {
                await self?.restorePairedFailed(identity: identity, generation: generation,
                                                revoked: false,
                                                message: error.localizedDescription)
            }
        }
    }

    func pair(invitationText: String) async throws {
        readinessError = nil

        guard !authBusy else { throw PairingError.invalidResponse }
        let generation = authGate.begin()
        authBusy = true
        defer { if authGate.accepts(generation) { authBusy = false } }
        let invitation = try PairingInvitation.parse(invitationText)
        let pending = pendingTailcatDirectory()
        try? FileManager.default.removeItem(at: pending)
        let bootstrap = try await makeNative(address: invitation.address,
                                             stateDirectory: pending,
                                             derpMap: invitation.derpMap)

        guard authGate.accepts(generation), demo == nil else { bootstrap.close(); return }
        let pendingIdentity = DeviceIdentity.createPending()
        let candidate = try pendingIdentity.bound(profileId: invitation.invite.profileId)
        do {
            try await invitationRedeemer(bootstrap.url, invitation, candidate, deviceName)
        } catch {
            bootstrap.close()
            try? FileManager.default.removeItem(at: pending)
            throw error
        }
        let identity = try candidate.persist()

        // Redeem is single-use. Persist the address, key and Tailcat state before
        // authentication so a transient challenge failure can resume on launch.
        bootstrap.close()
        let destination = tailcatDirectory(profileId: identity.profileId)
        try? FileManager.default.removeItem(at: destination)
        try FileManager.default.createDirectory(at: destination.deletingLastPathComponent(),
                                                withIntermediateDirectories: true)
        try FileManager.default.moveItem(at: pending, to: destination)
        storedProfileId = identity.profileId
        peerAddressString = invitation.address
        storedDERPMap = invitation.derpMap ?? ""

        let native = try await makeNative(address: invitation.address,
                                          stateDirectory: destination,
                                          derpMap: invitation.derpMap)

        guard authGate.accepts(generation), demo == nil else { native.close(); return }
        do {
            let session = try await sessionRenewer(native.url, identity)
            guard authGate.accepts(generation), demo == nil else { native.close(); return }

            connect(native: native, identity: identity, session: session, generation: generation)
        } catch {
            native.close()
            throw error
        }
    }

    func enterDemoMode() {
        guard !authBusy else { return }
        authGate.cancel()
        DocDisk.activate(profileId: "demo")
        demo = DemoDataset.standard()
        demoPinnedSessionIds = []
        phase = .ready
    }

    func signOut() {

        authGate.cancel()
        authBusy = false
        workspace?.stop()
        workspace = nil
        sessionStores.values.forEach { $0.stop() }
        sessionStores.removeAll()
        storeLastUsed.removeAll()
        config = nil
        tailcat?.close()
        tailcat = nil
        demo = nil
        demoPinnedSessionIds = []
        if !storedProfileId.isEmpty { DeviceIdentity.delete(profileId: storedProfileId) }
        DocDisk.closeProfile()
        storedProfileId = ""
        peerAddressString = ""
        storedDERPMap = ""
        readinessError = nil
        phase = .signedOut
    }

    private func connect(native: any TailcatClient, identity: DeviceIdentity,
                         session: PairingSession, generation: Int) {
        guard session.profileId == identity.profileId,
              session.deviceId == identity.deviceId,
              authGate.accepts(generation) else {
            readinessError = "Pairing response profile did not match the saved device key."
            native.close()
            return
        }
        DocDisk.activate(profileId: identity.profileId)
        DocDisk.prune(keep: 80)
        tailcat?.close()
        tailcat = native
        let profileId = identity.profileId
        let deviceId = session.deviceId
        let config = AppConfig(peerURL: native.url, profileId: profileId,
                               deviceId: deviceId, deviceName: deviceName,
                               identity: identity, session: session) { [weak self] in
            Task { @MainActor in
                self?.handleRevoked(profileId: profileId, deviceId: deviceId,
                                    generation: generation)
            }
        }
        self.config = config
        if let workspace,
           workspace.profileId == profileId, workspace.deviceId == deviceId {
            workspace.attachNetwork(config: config)
        } else {
            let store = WorkspaceStore(config: config)
            workspace = store
            store.start()
        }
        for store in sessionStores.values where store.deviceId == deviceId {
            store.attachNetwork(config: config, holdDial: store.isDialHeld)
        }
        startConnectivity()
        phase = .ready
    }

    private func handleRevoked(profileId: String, deviceId: String, generation: Int) {
        guard authGate.accepts(generation), config?.profileId == profileId,
              config?.deviceId == deviceId, storedProfileId == profileId else { return }
        signOut()
        readinessError = "This device was revoked. Pair it again to continue."
    }


    private func makeNative(address: String, stateDirectory: URL,
                            derpMap: String?) async throws -> any TailcatClient {
        let factory = nativeFactory
        return try await Task.detached(priority: .userInitiated) {
            try Task.checkCancellation()
            let native = try factory(address, stateDirectory, derpMap)
            if Task.isCancelled {
                native.close()
                throw CancellationError()
            }
            return native
        }.value
    }

    private func tailcatDirectory(profileId: String) -> URL {
        DocDisk.directory(profileId: profileId).appendingPathComponent("Tailcat", isDirectory: true)
    }

    private func pendingTailcatDirectory() -> URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("ZeronPairing", isDirectory: true)
            .appendingPathComponent(deviceId, isDirectory: true)
    }


    /// Wire the graced-connectivity recompute over the live stores (the
    /// engine's 1s compute_connectivity, phone edition).
    private func startConnectivity() {
        connectivity.registryConnected = { [weak self] in
            guard let self, self.demo == nil, let workspace = self.workspace else { return true }
            return workspace.connected
        }
        connectivity.registryRetryAt = { [weak self] in
            self?.workspace?.retryAt
        }
        connectivity.chatRooms = { [weak self] in
            guard let self else { return [] }
            return self.sessionStores.compactMap { id, store in
                store.roomActive ? (id: id, connected: store.connected,
                                    retryAt: store.retryAt) : nil
            }
        }
        connectivity.hasPendingSends = { [weak self] in
            self?.sessionStores.values.contains { !$0.pendingSends.isEmpty } ?? false
        }
        connectivity.start()
    }

    // MARK: Unified data accessors (demo or live — one path for views)

    var spaces: [Space] { demo?.spaces ?? workspace?.spaces ?? [] }
    var devices: [DeviceRow] { demo?.devices ?? workspace?.devices ?? [] }
    var executionDevices: [DeviceRow] { devices.filter(\.canHostSessions) }

    // "Connected" for the header spinner means "server state has reached this
    // session" — over the socket OR the HTTPS pull (which lands in ~1 RTT and
    // is the only transport airplane wifi permits).
    var connected: Bool { demo != nil || workspace?.connected == true || workspace?.synced == true }

    var overviewChats: [Chat] {
        if let demo {
            let liveIds = Set(demo.spaces.map(\.id))
            let live = demo.chats.filter { !$0.archived && ($0.spaceId.map(liveIds.contains) ?? true) }
            return sortPinnedFirst(live, pinnedSessionIds: demoPinnedSessionIds)
        }
        return workspace?.overviewChats ?? []
    }

    func chats(in spaceId: String) -> [Chat] {
        if let demo {
            return sortPinnedFirst(
                demo.chats.filter { !$0.archived && $0.spaceId == spaceId },
                pinnedSessionIds: demoPinnedSessionIds
            )
        }
        return workspace?.chats(in: spaceId) ?? []
    }

    func chat(id: String) -> Chat? {
        (demo?.chats ?? workspace?.chats)?.first { $0.id == id }
    }

    /// state.rs `space_for_chat` — nil for a dangling/missing space_id.
    func space(for chat: Chat) -> Space? {
        guard let spaceId = chat.spaceId else { return nil }
        return spaces.first { $0.id == spaceId }
    }

    func indicator(for chat: Chat) -> ChatIndicator {
        if let demo {
            return chatIndicator(chat: chat, live: effectiveStatus(demo.sessions[chat.id], now: nowMs()))
        }
        return workspace?.indicator(for: chat) ?? .idle
    }

    func changeRequest(for chat: Chat) -> ChangeRequestSummary? {
        if let demo { return demo.changeRequests[chat.id] }
        return workspace?.changeRequest(for: chat)
    }

    func spaceIndicator(_ spaceId: String) -> ChatIndicator? {
        chats(in: spaceId).map { indicator(for: $0) }.min { $0.rawValue < $1.rawValue }
    }

    func deviceName(_ deviceId: String) -> String {
        (demo?.devices ?? workspace?.devices)?.first { $0.id == deviceId }?.name ?? deviceId
    }

    func deviceOnline(_ deviceId: String) -> Bool {
        if let demo {
            guard let seen = demo.devices.first(where: { $0.id == deviceId })?.lastSeenAt else { return false }
            return nowMs() - seen < presenceFreshMs
        }
        return workspace?.deviceOnline(deviceId) ?? false
    }

    /// Live harness catalog from the selected execution device (Settings → Agents
    /// gates which agents a device offers); static pair when unreachable.
    func listHarnesses(deviceId: String) async -> [HarnessInfo] {
        if demo != nil {
            try? await Task.sleep(nanoseconds: 100_000_000)
            return HarnessCatalog.harnesses
        }
        if let live = await workspace?.listHarnesses(deviceId: deviceId),
           !live.isEmpty {
            return live
        }
        return HarnessCatalog.harnesses
    }

    /// Live model catalog from the selected execution device (the desktop's
    /// "catalog source = the device that runs the session" rule); static
    /// fallback when the device is unreachable.
    func listModels(deviceId: String, harness: String) async -> [ModelInfo] {
        if demo != nil {
            try? await Task.sleep(nanoseconds: 100_000_000)
            return HarnessCatalog.models(for: harness)
        }
        if let live = await workspace?.listModels(deviceId: deviceId, harness: harness),
           !live.isEmpty {
            let normalized = HarnessCatalog.normalize(harness: harness, models: live)
            if !normalized.isEmpty {
                _ = DocDisk.saveModels(normalized, deviceId: deviceId, harness: harness)
                return normalized
            }
        }
        if let cached = DocDisk.loadModels(deviceId: deviceId, harness: harness),
           !cached.isEmpty {
            return cached
        }
        return HarnessCatalog.models(for: harness)
    }

    /// Refs of the space's repo (git spaces only).
    func listRefs(space: Space) async -> [RepoRef]? {
        if let demo {
            try? await Task.sleep(nanoseconds: 120_000_000)
            return demo.listRefs(spacePath: space.path)
        }
        return await workspace?.listRefs(deviceId: space.deviceId, repoPath: space.path)
    }

    /// Draft-mode checkout switch: `git checkout` in the SPACE's folder.
    /// Returns an error message, or nil on success.
    func switchSpaceRef(space: Space, refName: String) async -> String? {
        if let demo {
            try? await Task.sleep(nanoseconds: 200_000_000)
            demo.switchRef(path: space.path, refName: refName)
            return nil
        }
        guard let workspace else { return "Not connected" }
        return await workspace.switchRef(deviceId: space.deviceId,
                                         repoPath: space.path, refName: refName)
    }

    /// Mid-session ref switch (desktop switch_session_ref): retarget onto the
    /// ref's existing worktree (row writes, no git), else checkout in the
    /// session's own cwd on the host. Returns an error message or nil.
    func switchSessionRef(chat: Chat, ref: RepoRef) async -> String? {
        guard let cwd = chat.cwd else { return "Session has no working folder" }
        if let worktree = ref.worktreePath {
            if worktree == cwd { return nil }  // already here
            if let demo {
                if let ix = demo.chats.firstIndex(where: { $0.id == chat.id }) {
                    demo.chats[ix].cwd = worktree
                    demo.chats[ix].branch = ref.name
                }
                return nil
            }
            workspace?.setChatCheckout(chatId: chat.id, cwd: worktree, branch: ref.name)
            return nil
        }
        if let demo {
            try? await Task.sleep(nanoseconds: 200_000_000)
            demo.switchRef(path: cwd, refName: ref.name)
            if let ix = demo.chats.firstIndex(where: { $0.id == chat.id }) {
                demo.chats[ix].branch = ref.name
            }
            return nil
        }
        guard let workspace else { return "Not connected" }
        let error = await workspace.switchRef(deviceId: chat.deviceId,
                                              repoPath: cwd, refName: ref.name)
        if error == nil {
            // The host's HEAD watcher reconciles chat.branch eventually;
            // stamp it optimistically so the UI answers immediately.
            workspace.setChatCheckout(chatId: chat.id, cwd: cwd, branch: ref.name)
        }
        return error
    }

    /// CreateWorktree off the base ref; returns the new worktree's path.
    func createWorktree(space: Space, base: String) async -> String? {
        if let demo {
            try? await Task.sleep(nanoseconds: 250_000_000)
            return demo.createWorktree(spacePath: space.path, base: base)
        }
        return await workspace?.createWorktree(deviceId: space.deviceId,
                                               spaceId: space.id,
                                               repoPath: space.path, branch: base)
    }

    @discardableResult
    func createChat(space: Space, config chatConfig: ChatConfig,
                    branch: String? = nil, cwd: String? = nil) -> String? {
        createChat(deviceId: space.deviceId, space: space, config: chatConfig,
                   branch: branch, cwd: cwd)
    }

    @discardableResult
    func createProjectlessChat(deviceId: String, config: ChatConfig) -> String? {
        guard executionDevices.contains(where: { $0.id == deviceId }) else { return nil }
        return createChat(deviceId: deviceId, space: nil, config: config)
    }

    private func createChat(deviceId: String, space: Space?, config chatConfig: ChatConfig,
                            branch: String? = nil, cwd: String? = nil) -> String? {
        if let demo {
            let id = "chat-\(UUID().uuidString.lowercased().prefix(8))"
            demo.chats.append(Chat(id: id, deviceId: deviceId, title: nil, archived: false,
                                   cwd: space.map { cwd ?? $0.path } ?? "~",
                                   branch: branch, checkoutId: nil,
                                   config: chatConfig, lastMessagePreview: nil, lastMessageAt: nil,
                                   createdAt: nowMs(), spaceId: space?.id, lastSeenAt: nowMs(),
                                   roomGen: 2))
            return id
        }
        if let space {
            return workspace?.createChat(space: space, config: chatConfig, branch: branch, cwd: cwd)
        }
        return workspace?.createProjectlessChat(deviceId: deviceId, config: chatConfig)
    }

    /// Browse folders on a remote device (the desktop add-space palette's data
    /// path). Demo mode serves a canned tree; live mode asks the device over
    /// the relay.
    func listFolders(deviceId: String, path: String?) async -> FolderListing? {
        if let demo {
            try? await Task.sleep(nanoseconds: 120_000_000)  // feel like a network hop
            let target = path ?? demo.homePath(deviceId: deviceId)
            return demo.listFolders(deviceId: deviceId, path: target)
        }
        return await workspace?.listFolders(deviceId: deviceId, path: path)
    }

    @discardableResult
    func createSpace(deviceId: String, path: String, gitDetected: Bool = false) async -> String? {
        if let demo {
            if let existing = demo.spaces.first(where: { $0.deviceId == deviceId && $0.path == path }) {
                return existing.id
            }
            let id = "space-\(UUID().uuidString.lowercased().prefix(8))"
            demo.spaces.append(Space(id: id, deviceId: deviceId, path: path, name: nil,
                                     gitDetected: gitDetected, gitCheckedAt: nil, checkoutId: nil,
                                     createdAt: nowMs()))
            return id
        }
        return await workspace?.createSpace(deviceId: deviceId, path: path, gitDetected: gitDetected)
    }

    /// Archived chats under the same scope as the list above the shelf.
    func archivedChats(in spaceId: String? = nil) -> [Chat] {
        if let demo {
            return sortActive(demo.chats.filter {
                $0.archived && (spaceId == nil || $0.spaceId == spaceId)
            })
        }
        return workspace?.archivedChats(in: spaceId) ?? []
    }

    func archive(chatId: String) { setArchived(chatId: chatId, archived: true) }
    func unarchive(chatId: String) { setArchived(chatId: chatId, archived: false) }

    var pinsReady: Bool {
        if demo != nil { return true }
        guard let workspace else { return false }
        return workspace.synced || workspace.sidebarPreferencesInitialized
    }

    func isPinned(chatId: String) -> Bool {
        (demo != nil ? demoPinnedSessionIds : workspace?.pinnedSessionIds ?? []).contains(chatId)
    }

    func setPinned(chatId: String, pinned: Bool) {
        if demo != nil {
            if pinned {
                guard !demoPinnedSessionIds.contains(chatId),
                      demoPinnedSessionIds.count < WorkspaceStore.maxSidebarPins else { return }
                demoPinnedSessionIds.append(chatId)
            } else {
                demoPinnedSessionIds.removeAll { $0 == chatId }
            }
            return
        }
        workspace?.setPinned(chatId: chatId, pinned: pinned)
    }

    private func setArchived(chatId: String, archived: Bool) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].archived = archived
            }
            return
        }
        workspace?.setArchived(chatId: chatId, archived: archived)
    }

    func setChatConfig(chatId: String, config: ChatConfig) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].config = config
            }
            return
        }
        workspace?.setChatConfig(chatId: chatId, config: config)
    }

    func markSeen(chatId: String) {
        if let demo {
            if let ix = demo.chats.firstIndex(where: { $0.id == chatId }) {
                demo.chats[ix].lastSeenAt = nowMs()
            }
            return
        }
        workspace?.markSeen(chatId: chatId)
    }

    /// Persist every open doc now (app backgrounding).
    func flushDocs() {
        workspace?.flushToDisk()
        let orderedIDs = Array(Self.evictionOrder(lastUsed: storeLastUsed) { _ in false }.reversed())
        let knownIDs = Set(orderedIDs)
        let missingIDs = sessionStores.keys.filter { !knownIDs.contains($0) }
        let stores = (orderedIDs + missingIDs).compactMap { sessionStores[$0] }
        guard !stores.isEmpty else { return }

        let state = BackgroundFlushState()
        let identifier = UIApplication.shared.beginBackgroundTask(withName: "zeron.flushDocs") {
            let identifier = state.cancel()
            if identifier != .invalid {
                UIApplication.shared.endBackgroundTask(identifier)
            }
        }
        state.setIdentifier(identifier)
        stores.forEach { $0.retireSaverTimers() }
        Task { @MainActor [stores, state] in
            for store in stores where !state.isCancelled && !store.stopped {
                await store.flushToDiskAsync()
            }
            let identifier = state.finish()
            if identifier != .invalid {
                UIApplication.shared.endBackgroundTask(identifier)
            }
        }
    }

    /// Foreground hook: kick every room NOW (see ChatRoomClient.kick) — after
    /// a suspension the workspace room in particular stayed dead while chat
    /// views reconnected on open, freezing sidebar rows and Working
    /// indicators against perfectly live transcripts (2026-08-04). Also the
    /// focus fast path probes the peer through Tailcat (3s) and broadcasts the
    /// online event on success, so every PARKED backoff (not just the rooms
    /// the kick reaches) lands a redial in ~1 RTT.
    func foregrounded() {

        if tailcat == nil { retrySavedIdentity() }
        kickAllRooms()
        probeEdgeHealth()
    }

    private func probeEdgeHealth() {
        guard tailcat != nil, let config, demo == nil else { return }
        Task.detached {
            var request = URLRequest(url: config.peerURL.appending(path: "health"))
            request.timeoutInterval = 3
            guard let (_, response) = try? await URLSession.shared.data(for: request),
                  (response as? HTTPURLResponse)?.statusCode == 200 else { return }
            OnlineBus.shared.notifyOnline()
        }
    }

    private func kickAllRooms() {
        workspace?.kickRoom()
        // Deliver any roomGen flips that landed while the store had no open
        // view, then kick every room — registry first and instantly, chat
        // rooms trickled one per 200ms in attention order. Post-suspend and
        // path-recovery kicks redial dead sockets; a simultaneous N-socket
        // redial competed with the registry (the sidebar the user is
        // actually looking at) on thin links.
        if let workspace {
            for chat in workspace.chats {
                sessionStores[chat.id]?.updateRoomGen(chat.roomGen)
            }
        }
        var delay: UInt64 = 0
        var kicked = Set<String>()
        for chat in overviewChats {
            // Dial-held stores stay held: a kick force-dials, and sweeping 46
            // of them on every foreground/path flap is the stampede the warm
            // cap exists to prevent. Held chats reconnect on open.
            guard let store = sessionStores[chat.id],
                  !store.isDialHeld || !store.outbox.isEmpty else { continue }
            kicked.insert(chat.id)
            scheduleKick(chatId: chat.id, afterNs: delay)
            delay += 200_000_000
        }
        for (id, store) in sessionStores
            where !kicked.contains(id) && (!store.isDialHeld || !store.outbox.isEmpty) {
            scheduleKick(chatId: id, afterNs: delay)
            delay += 200_000_000
        }
    }

    private func scheduleKick(chatId: String, afterNs delay: UInt64) {
        Task { @MainActor [weak self] in
            if delay > 0 { try? await Task.sleep(nanoseconds: delay) }
            guard let self, let store = self.sessionStores[chatId] else { return }
            store.kickRoom()
        }
    }

    /// Kick rooms the moment the network path recovers or hops interfaces
    /// (wifi drop-and-return while foregrounded, wifi→cellular handover).
    /// Without this the clients sleep out their full reconnect backoff — up
    /// to 30s of dead sidebar on exactly the flaky networks (airplane wifi)
    /// where the OS knows recovery happened the instant it did. Kicks are
    /// idempotent: fresh backoff + immediate redial or a deadline-checked
    /// probe on a session that looks alive.
    private func startPathMonitor() {
        guard pathMonitor == nil else { return }
        let monitor = NWPathMonitor()
        monitor.pathUpdateHandler = { [weak self] path in
            // net_path.rs semantics: only a definitive "unsatisfied" parks —
            // requiresConnection/other stay optimistic (a confused monitor
            // can only make us dial too much, never go silent). Every
            // satisfied report also broadcasts online: satisfied→satisfied
            // updates are interface handovers (wifi→cellular), and the old
            // sockets are dead on the new path; redundant kicks are free
            // because waiters drain stale events.
            OnlineBus.shared.setPathOnline(path.status != .unsatisfied)
            if path.status == .satisfied {
                OnlineBus.shared.notifyOnline()

                Task { @MainActor [weak self] in
                    guard let self, self.tailcat == nil else { return }
                    self.retrySavedIdentity()
                }
            }
            // Interface set is part of the key: a satisfied→satisfied hop
            // (wifi→cellular) silently kills established sockets too.
            let key = path.status == .satisfied
                ? "up:" + path.availableInterfaces.map(\.name).sorted().joined(separator: ",")
                : "down"
            Task { @MainActor [weak self] in
                guard let self else { return }
                self.connectivity.setPathOffline(path.status == .unsatisfied)
                let previous = self.lastPathKey
                self.lastPathKey = key
                // First callback reports the initial state — nothing to revive.
                guard let previous, previous != key, path.status == .satisfied else { return }
                roomLog.info("network path recovered (\(key, privacy: .public)); kicking rooms")
                self.kickAllRooms()
            }
        }
        monitor.start(queue: DispatchQueue(label: "zeron.path-monitor"))
        pathMonitor = monitor
    }

    /// Diagnostics access (live e2e probe).
    var diagnosticsConfig: AppConfig? { config }

    // MARK: Session stores

    func sessionStore(for chat: Chat) -> SessionStore? {
        if let demo { return demo.sessionStore(for: chat.id) }
        guard let config else { return nil }
        if let existing = sessionStores[chat.id] {
            if existing.stopped {
                sessionStores.removeValue(forKey: chat.id)
                storeLastUsed.removeValue(forKey: chat.id)
            } else {
                touchStore(chat.id)
                existing.hostDeviceId = chat.deviceId
                // The registry flip to chat2 can land while the store is open —
                // views re-derive `chat` from the registry on every change, so
                // this accessor is the flip's delivery path.
                existing.updateRoomGen(chat.roomGen)
                // An open view wants live sync NOW — any preload dial-hold ends.
                existing.releaseDial()
                return existing
            }
        }
        let store = SessionStore(chatId: chat.id, config: config)
        store.onPersisted = { [weak self] in self?.evictColdStores() }
        store.hostDeviceId = chat.deviceId
        store.hostLiveness = { [weak self] deviceId in
            self?.workspace?.peerLiveness(deviceId) ?? .unknown
        }
        sessionStores[chat.id] = store
        touchStore(chat.id)
        if tailcat != nil { store.start() }
        store.updateRoomGen(chat.roomGen)
        return store
    }

    // MARK: Delivery truth (state.rs chat_delivery_degraded / send_* ports)

    /// Queued-attachment version gate (composer.rs QUEUED_ATTACHMENTS_MIN):
    /// the host must defer commands with pending:// refs, or the send would
    /// dispatch with unresolvable paths.
    static let queuedAttachmentsMin = (0, 2, 12)

    func hostSupportsQueuedAttachments(_ chat: Chat) -> Bool {
        hostSupportsQueuedAttachmentsOn(deviceId: chat.deviceId)
    }

    func hostSupportsQueuedAttachmentsOn(deviceId: String) -> Bool {
        guard demo == nil else { return false }
        return workspace?.deviceVersionAtLeast(deviceId, Self.queuedAttachmentsMin) ?? false
    }

    /// The visible message queue is a personal-cut capability, not a semver
    /// promise: an upstream host can have the same version without its doc/RPC
    /// surface. Attachments require the stronger queue capability.
    func hostSupportsMessageQueue(_ chat: Chat, attachments: Bool = false) -> Bool {
        guard demo == nil else { return false }
        let capability = attachments
            ? EngineCapability.messageQueueAttachmentsV1
            : EngineCapability.messageQueueV1
        return workspace?.deviceSupports(chat.deviceId, capability) ?? false
    }

    func hostSupportsCleanQueueAttachmentText(_ chat: Chat) -> Bool {
        guard demo == nil else { return false }
        return workspace?.deviceSupports(
            chat.deviceId,
            EngineCapability.messageQueueCleanAttachmentTextV1
        ) ?? false
    }

    func hostSupportsQueueEditLease(_ chat: Chat) -> Bool {
        guard demo == nil else { return false }
        return workspace?.deviceSupports(chat.deviceId, EngineCapability.messageQueueEditLeaseV1)
            ?? false
    }

    /// Whether a send to this chat would queue rather than deliver promptly:
    /// OS offline, the chat's room degraded (graced), or the host device
    /// presence-dark. Every chat is remote-hosted on the phone — there is no
    /// "locally hosted, never degraded" branch.
    func chatDeliveryDegraded(_ chat: Chat) -> Bool {
        guard demo == nil else { return false }
        if connectivity.state == .offline { return true }
        if let store = sessionStores[chat.id], store.roomActive {
            if connectivity.degradedChats.contains(chat.id) { return true }
        } else if connectivity.state != .connected {
            return true
        }
        if !deviceOnline(chat.deviceId) { return true }
        return false
    }

    /// The user-visible truth of a chat's oldest unadopted send. `failed`
    /// (unadopted past the 2-minute grace, with a retry affordance) wins over
    /// `queued` (pending on a degraded path); a healthy in-flight send reads
    /// `sending`. nil = nothing pending.
    func sendState(for chat: Chat, now: Int64 = nowMs()) -> SendState? {
        guard demo == nil, let store = sessionStores[chat.id],
              let oldest = store.pendingSends.map(\.started).min() else { return nil }
        if now - oldest > undeliveredGraceMs { return .failed }
        if chatDeliveryDegraded(chat) { return .queued }
        return .sending
    }

    func releaseSessionStore(chatId: String) {
        guard let store = sessionStores[chatId] else { return }
        store.detachView()
        touchStore(chatId)
        evictColdStores()
    }

    func attachSessionView(chatId: String) {
        guard demo == nil else { return }
        sessionStores[chatId]?.attachView()
    }

    /// Warm every non-archived session: stores hydrate from disk instantly
    /// so opening a session never shows a loading state. The room DIALS are
    /// held and released one per 300ms in attention order — N simultaneous
    /// TLS handshakes at launch competed with the registry dial for a thin
    /// uplink (and, pre-single-flight, raced N token refreshes), which was
    /// the cold-open "connecting…" stall. Opening a session releases its
    /// hold immediately (sessionStore(for:) above).
    /// Sessions that keep a live socket without an open view. Everything else
    /// hydrates from disk but dials on demand: 46 background joins (TLS +
    /// hello + state each) drowned a 450kbps link for tens of seconds at
    /// every cold open and network kick, for transcripts nobody was reading —
    /// sidebar status (Working, presence, titles) rides the registry room, so
    /// an undialed chat's row stays live regardless, and opening it releases
    /// its dial instantly.
    static let warmDialCap = 8
    static let warmStoreCap = 12
    static let residentByteBudget = 80 * 1024 * 1024
    static let residentBytesPerSnapshotByte = 6
    static let residentFloorBytes = 512 * 1024

    nonisolated static func residentEstimate(snapshotBytes: Int) -> Int {
        max(snapshotBytes * residentBytesPerSnapshotByte, residentFloorBytes)
    }

    nonisolated static func warmPreloadIDs(
        chats: [Chat],
        hasPendingOutbox: (String) -> Bool,
        cap: Int,
        snapshotBytes: (String) -> Int = { _ in 0 },
        byteBudget: Int = .max
    ) -> [String] {
        let limit = max(0, cap)
        var ids: [String] = []
        var selected = Set<String>()
        var bytes = 0
        for chat in chats where selected.insert(chat.id).inserted {
            let pending = hasPendingOutbox(chat.id)
            let estimate = residentEstimate(snapshotBytes: snapshotBytes(chat.id))
            if pending {
                ids.append(chat.id)
                bytes += estimate
            } else if ids.count < limit, bytes + estimate <= byteBudget {
                ids.append(chat.id)
                bytes += estimate
            }
        }
        return ids
    }

    nonisolated static func warmDialIDs(
        ids: [String],
        hasPendingOutbox: (String) -> Bool,
        cap: Int
    ) -> [String] {
        let limit = max(0, cap)
        var released: [String] = []
        var selected = Set<String>()
        for id in ids.prefix(limit) where selected.insert(id).inserted {
            released.append(id)
        }
        for id in ids.dropFirst(limit)
            where hasPendingOutbox(id) && selected.insert(id).inserted {
            released.append(id)
        }
        return released
    }

    nonisolated static func evictionOrder(
        lastUsed: [String: UInt64],
        protected: (String) -> Bool
    ) -> [String] {
        lastUsed.keys
            .filter { !protected($0) }
            .sorted {
                let lhs = lastUsed[$0] ?? 0
                let rhs = lastUsed[$1] ?? 0
                return lhs == rhs ? $0 < $1 : lhs < rhs
            }
    }

    nonisolated static func evictionPlan(
        lastUsed: [String: UInt64],
        estimates: [String: Int],
        protected: (String) -> Bool,
        countCap: Int,
        byteBudget: Int
    ) -> [String] {
        let newest = lastUsed.max { $0.value < $1.value }?.key
        var remainingCount = estimates.count
        var remainingBytes = estimates.values.reduce(0, +)
        guard remainingCount > countCap || remainingBytes > byteBudget else { return [] }
        var plan: [String] = []
        let order = evictionOrder(lastUsed: lastUsed) { id in
            id == newest || protected(id)
        }
        for id in order {
            guard remainingCount > countCap || remainingBytes > byteBudget else { break }
            guard let estimate = estimates[id] else { continue }
            plan.append(id)
            remainingCount -= 1
            remainingBytes -= estimate
        }
        return plan
    }

    private func touchStore(_ id: String) {
        usageClock &+= 1
        storeLastUsed[id] = usageClock
        let warmIDs = Set(storeLastUsed.sorted { $0.value > $1.value }
            .prefix(3).map(\.key))
        for (storeID, store) in sessionStores {
            store.keepsParseCacheWarm = warmIDs.contains(storeID)
        }
    }

    private func storeIsProtected(_ store: SessionStore) -> Bool {
        !store.pendingSends.isEmpty
            || !store.outbox.isEmpty
            || store.entries.last?.status == .streaming
    }

    private func evictColdStores() {
        let estimates = sessionStores.mapValues { Self.residentEstimate(snapshotBytes: $0.snapshotBytes) }
        let plan = Self.evictionPlan(
            lastUsed: storeLastUsed,
            estimates: estimates,
            protected: { [weak self] id in
            guard let self, let store = self.sessionStores[id] else { return false }
            return self.storeIsProtected(store)
            },
            countCap: Self.warmStoreCap,
            byteBudget: Self.residentByteBudget
        )
        guard !plan.isEmpty else { return }
        var removed = 0
        for id in plan {
            guard let store = sessionStores.removeValue(forKey: id) else { continue }
            store.stop()
            storeLastUsed.removeValue(forKey: id)
            removed += 1
        }
        if removed > 0 {
            roomLog.info("session store eviction removed \(removed, privacy: .public) cold store(s)")
        }
    }

    private func evictForMemoryWarning() {
        let newest = storeLastUsed.max { $0.value < $1.value }?.key
        let before = sessionStores.count
        let order = Self.evictionOrder(lastUsed: storeLastUsed) { [weak self] id in
            guard let self, let store = self.sessionStores[id] else { return true }
            return id == newest || self.storeIsProtected(store)
        }
        for id in order {
            guard let store = sessionStores.removeValue(forKey: id) else { continue }
            store.stop()
            storeLastUsed.removeValue(forKey: id)
        }
        roomLog.info("memory warning evicted \(before - self.sessionStores.count, privacy: .public) of \(before, privacy: .public) session store(s)")
    }

    func preloadSessions() {
        guard demo == nil, let config else { return }
        var stagger: UInt64 = 0
        let preloadIDs = Self.warmPreloadIDs(
            chats: overviewChats,
            hasPendingOutbox: { DocDisk.chat2HasPendingOutbox(id: $0) },
            cap: Self.warmStoreCap,
            snapshotBytes: { DocDisk.chat2SnapshotSize(id: $0) },
            byteBudget: Self.residentByteBudget
        )
        for chat in overviewChats where preloadIDs.contains(chat.id) {
            if sessionStores[chat.id]?.stopped == true {
                sessionStores.removeValue(forKey: chat.id)
                storeLastUsed.removeValue(forKey: chat.id)
            }
            guard sessionStores[chat.id] == nil else { continue }
            let store = SessionStore(chatId: chat.id, config: config)
            store.onPersisted = { [weak self] in self?.evictColdStores() }
            store.hostDeviceId = chat.deviceId
            store.hostLiveness = { [weak self] deviceId in
                self?.workspace?.peerLiveness(deviceId) ?? .unknown
            }
            sessionStores[chat.id] = store
            touchStore(chat.id)
            if tailcat != nil {
                store.start(holdDial: true)
            } else {
                store.holdNetworkUntilReleased()
            }
            store.updateRoomGen(chat.roomGen)
        }
        let warmDialIDs = Self.warmDialIDs(
            ids: preloadIDs,
            hasPendingOutbox: { sessionStores[$0]?.outbox.isEmpty == false },
            cap: Self.warmDialCap
        )
        for id in warmDialIDs {
            guard let chat = overviewChats.first(where: { $0.id == id }),
                  let store = sessionStores[id], store.isDialHeld else { continue }
            let delay = stagger
            Task { @MainActor [weak self, weak store] in
                guard let self else { return }
                // The registry (the sidebar the user is looking at) gets the
                // pipe to itself first: on a 240kbps link, warm chat dials
                // racing the registry's own handshake+state pushed the
                // connect spinner from ~1.5s to ~7s (NLC Edge, 2026-08-17).
                // An open view still dials instantly via releaseDial.
                let start = DispatchTime.now()
                while !(self.workspace?.connected ?? false),
                      DispatchTime.now().uptimeNanoseconds &- start.uptimeNanoseconds < 10_000_000_000 {
                    try? await Task.sleep(nanoseconds: 200_000_000)
                }
                if delay > 0 { try? await Task.sleep(nanoseconds: delay) }
                guard let store, self.sessionStores[chat.id] === store else { return }
                store.releaseDial()
            }
            stagger += 300_000_000
        }
    }
}

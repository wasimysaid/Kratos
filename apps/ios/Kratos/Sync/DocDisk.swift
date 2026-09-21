// Profile-scoped on-device Loro persistence. Each paired identity starts in
// its own namespace and only reads snapshots written inside that namespace.

import Foundation
import Loro

enum DocDisk {
    private static let lock = NSLock()
    private static var activeProfileId: String?

    private static var support: URL {
        FileManager.default.urls(for: .applicationSupportDirectory,
                                 in: .userDomainMask)[0]
    }


    static func activate(profileId: String) {
        precondition(!profileId.isEmpty)
        lock.withLock { activeProfileId = profileId }
        _ = directory
    }

    static func deactivate() {
        lock.withLock { activeProfileId = nil }
    }

    static var directory: URL {
        guard let profile = lock.withLock({ activeProfileId }) else {
            preconditionFailure("DocDisk used before a profile was activated")
        }
        return directory(profileId: profile)
    }

    static func directory(profileId: String) -> URL {
        let safe = profileId.data(using: .utf8)!.base64URLEncodedString()
        let base = support.appendingPathComponent("KratosProfiles", isDirectory: true)
            .appendingPathComponent(safe, isDirectory: true)
        try? FileManager.default.createDirectory(at: base, withIntermediateDirectories: true)
        return base
    }



    /// The workspace registry's persisted blob ({rows, cursor, gcFloor,
    /// clock, pending} JSON; session docs use chat2 Loro snapshots.
    static func registryURL(orgId: String, userId: String) -> URL {
        directory.appendingPathComponent("registry1_\(orgId)_\(userId).json")
    }

    // MARK: chat2 lineage snapshots (docs/chat2-sync.md C2)

    /// `c2_<id>.loro` = 8-byte magic + UInt64 LE room cursor + snapshot,
    /// written atomically so doc content and cursor cannot diverge.
    private static let chat2Magic = Data("C2SNAP01".utf8)

    static func chat2URL(for id: String) -> URL {
        let safe = id.replacingOccurrences(of: "/", with: "_")
        return directory.appendingPathComponent("c2_\(safe).loro")
    }


    /// Import the chat2 snapshot; returns its cursor, or nil when absent or
    /// unreadable (caller starts fresh at cursor 0 — the room re-serves).
    static func loadChat2(into doc: LoroDoc, id: String) -> UInt64? {
        guard let data = try? Data(contentsOf: chat2URL(for: id)),
              data.count >= 16, data.prefix(8) == chat2Magic else { return nil }
        var cursor: UInt64 = 0
        for (ix, byte) in data.subdata(in: 8..<16).enumerated() {
            cursor |= UInt64(byte) << (8 * ix)
        }
        guard data.count > 16 else { return cursor }
        guard (try? doc.importWith(bytes: data.subdata(in: 16..<data.count),
                                   origin: "disk")) != nil else { return nil }
        return cursor
    }

    /// Atomically persist the chat2 doc snapshot + its room cursor.
    static func saveChat2(doc: LoroDoc, id: String, cursor: UInt64) {
        guard let snapshot = try? doc.export(mode: .snapshot) else { return }
        var data = chat2Magic
        var le = cursor.littleEndian
        withUnsafeBytes(of: &le) { data.append(contentsOf: $0) }
        data.append(snapshot)
        try? data.write(to: chat2URL(for: id), options: .atomic)
    }

    /// LRU-prune active chat2 session snapshots; registry and unrelated files stay untouched.
    static func prune(keep: Int) {
        let fm = FileManager.default
        guard let files = try? fm.contentsOfDirectory(at: directory,
                                                      includingPropertiesForKeys: [.contentModificationDateKey])
        else { return }
        let sessions = files.filter {
            $0.pathExtension == "loro" && $0.lastPathComponent.hasPrefix("c2_")
        }
        guard sessions.count > keep else { return }
        let sorted = sessions.sorted {
            let a = (try? $0.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            let b = (try? $1.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate) ?? .distantPast
            return a > b
        }
        for stale in sorted.dropFirst(keep) {
            try? fm.removeItem(at: stale)
        }
    }

    /// Sign-out closes this profile without deleting it. A later re-pair to
    /// the same profile can reopen its cache; unrelated profiles cannot.
    static func closeProfile() {
        deactivate()
    }
}

/// Debounced snapshot persistence shared by the doc stores: poke on every
/// change; `save` runs ~1.5s after the last poke, and `flush` forces it
/// (backgrounding, store teardown). The closure captures whatever must be
/// written together (e.g. a chat2 doc AND its cursor — one atomic file).
@MainActor
final class DocSaver {
    private let save: () -> Void
    private var generation = 0
    private var dirty = false

    init(save: @escaping () -> Void) {
        self.save = save
    }

    func poke() {
        dirty = true
        generation += 1
        let expected = generation
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            guard let self, self.generation == expected else { return }
            self.flush()
        }
    }

    func flush() {
        guard dirty else { return }
        dirty = false
        save()
    }
}

/// DocSaver's registry twin: debounced persistence for the registry blob.
/// Poke on every mutation; the blob writes ~1.5s after the last poke, and
/// `flush` forces it (backgrounding, store teardown).
@MainActor
final class RegistrySaver {
    private let url: URL
    private let data: () -> Data?
    private var generation = 0
    private var dirty = false

    init(url: URL, data: @escaping () -> Data?) {
        self.url = url
        self.data = data
    }

    func poke() {
        dirty = true
        generation += 1
        let expected = generation
        Task { @MainActor [weak self] in
            try? await Task.sleep(nanoseconds: 1_500_000_000)
            guard let self, self.generation == expected else { return }
            self.flush()
        }
    }

    func flush() {
        guard dirty else { return }
        dirty = false
        guard let data = data() else { return }
        try? data.write(to: url, options: .atomic)
    }
}

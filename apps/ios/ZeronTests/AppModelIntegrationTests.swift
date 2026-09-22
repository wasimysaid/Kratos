import CryptoKit

import Foundation
import Loro
import XCTest
@testable import Zeron

private final class TestTailcatClient: TailcatClient, @unchecked Sendable {
    let url = URL(string: "http://127.0.0.1:49152")!
    func close() {}
}


private actor NativeStartProbe {
    private var started = false
    func markStarted() { started = true }
    func hasStarted() -> Bool { started }
}

final class AppModelIntegrationTests: XCTestCase {
    @MainActor
    func testRestoreProjectsRealRegistryAndTranscriptCacheBeforeSlowNativeStartup() async throws {
        let profile = "cache-\(UUID().uuidString.lowercased())"
        let identity = try identity(profile: profile, seed: 1)
        DocDisk.activate(profileId: profile)
        try seedRegistry(profile: profile, device: identity.deviceId, chatId: "cached-chat")
        try seedTranscript(chatId: "cached-chat")

        let startProbe = NativeStartProbe()
        let startTime = Date()

        let model = AppModel(nativeFactory: { _, _, _ in
            Task { await startProbe.markStarted() }
            Thread.sleep(forTimeInterval: 0.6)
            return TestTailcatClient()

        // The production bug blocked this actor inside Go StartClient.
        }, sessionRenewer: { _, identity in
            PairingSession(token: "live", expiresAt: Int64.max,
                           principal: AuthPrincipal(profileId: identity.profileId,
                                                    deviceId: identity.deviceId))
        }, identityLoader: { requested in requested == profile ? identity : nil })

        defer { model.signOut() }
        model.storedProfileId = profile
        model.peerAddressString = "peer.test:443"
        model.storedDeviceId = identity.deviceId

        model.restore()
        while !(await startProbe.hasStarted()), Date().timeIntervalSince(startTime) < 1 {
            try await Task.sleep(nanoseconds: 5_000_000)
        }
        let nativeDidStart = await startProbe.hasStarted()
        XCTAssertTrue(nativeDidStart)
        XCTAssertLessThan(Date().timeIntervalSince(startTime), 0.3,
                          "native creation must begin without occupying the main actor")
        XCTAssertEqual(model.workspace?.chats.first?.id, "cached-chat")
        let chat = try XCTUnwrap(model.workspace?.chats.first)
        let session = try XCTUnwrap(model.sessionStore(for: chat))

        let actorResponsive = expectation(description: "main actor remained responsive")
        Task { @MainActor in actorResponsive.fulfill() }
        await fulfillment(of: [actorResponsive], timeout: 0.3)

        try await waitUntil { session.entries.first?.id == "cached-message" }
        XCTAssertEqual(session.entries.first?.id, "cached-message")
        guard case .text(_, let cachedText)? = session.entries.first?.parts.first else {
            XCTFail("cached transcript text was not projected")
            return
        }
        XCTAssertEqual(cachedText, "available offline")
    }

    @MainActor
    func testRevocationFromOldInstalledGenerationCannotClearNewProfile() async throws {
        let first = try identity(profile: "first-\(UUID().uuidString)", seed: 2)
        let second = try identity(profile: "second-\(UUID().uuidString)", seed: 3)
        let identities = [first.profileId: first, second.profileId: second]
        let model = AppModel(nativeFactory: { _, _, _ in TestTailcatClient() },
                             sessionRenewer: { _, identity in
            PairingSession(token: identity.profileId, expiresAt: Int64.max,
                           principal: AuthPrincipal(profileId: identity.profileId,
                                                    deviceId: identity.deviceId))
        }, identityLoader: { identities[$0] })

        defer { model.signOut() }
        model.storedProfileId = first.profileId
        model.peerAddressString = "first.test:443"
        model.storedDeviceId = first.deviceId
        model.restore()
        try await waitUntil { model.diagnosticsConfig?.peerURL.port == 49_152 }
        let staleConfig = try XCTUnwrap(model.diagnosticsConfig)

        model.signOut()
        model.storedProfileId = second.profileId
        model.peerAddressString = "second.test:443"
        model.storedDeviceId = second.deviceId
        model.retrySavedIdentity()
        try await waitUntil {
            model.diagnosticsConfig?.profileId == second.profileId
                && model.diagnosticsConfig?.peerURL.port == 49_152
        }

        staleConfig.reportUnauthorized()
        try await Task.sleep(nanoseconds: 50_000_000)
        XCTAssertEqual(model.diagnosticsConfig?.profileId, second.profileId)
        XCTAssertTrue(model.workspace != nil)
    }

    @MainActor
    func testFailedRedeemPreservesSavedIdentityAndRetryUsesIt() async throws {
        let profile = "redeem-failure-\(UUID().uuidString.lowercased())"
        let saved = try identity(profile: profile, seed: 6)
        _ = try saved.persist()
        var renewDeviceIds: [String] = []
        let model = AppModel(nativeFactory: pairingNativeFactory(),
                             sessionRenewer: { _, identity in
            renewDeviceIds.append(identity.deviceId)
            return PairingSession(token: "recovered", expiresAt: Int64.max,
                                  principal: AuthPrincipal(profileId: identity.profileId,
                                                           deviceId: identity.deviceId))
        }, invitationRedeemer: { _, invitation, candidate, _ in
            XCTAssertEqual(candidate.profileId, invitation.invite.profileId,
                           "the candidate must sign with the invited profile id")
            XCTAssertEqual(DeviceIdentity.load(profileId: profile), saved,
                           "redeem must run before the candidate touches Keychain")
            throw PairingError.http(409, "invite already consumed")
        })
        defer { model.signOut() }
        model.storedProfileId = profile
        model.peerAddressString = "saved-peer.test:443"
        model.storedDeviceId = saved.deviceId

        do {
            try await model.pair(invitationText: try invitationText(profile: profile))
            XCTFail("consumed invitation should fail")
        } catch PairingError.http(let status, _) {
            XCTAssertEqual(status, 409)
        }

        XCTAssertEqual(DeviceIdentity.load(profileId: profile), saved,
                       "failed redemption must leave the recoverable key unchanged")
        model.retrySavedIdentity()
        try await waitUntil { model.diagnosticsConfig?.peerURL.port == 49_152 }
        XCTAssertEqual(renewDeviceIds, [saved.deviceId])
        XCTAssertEqual(model.diagnosticsConfig?.deviceId, saved.deviceId)
    }

    @MainActor
    func testRedeemedCandidateIsSavedBeforeTransientRenewalFailureAndCanRetry() async throws {
        let profile = "redeem-success-\(UUID().uuidString.lowercased())"
        var redeemedDeviceId: String?
        var renewAttempts = 0
        let model = AppModel(nativeFactory: pairingNativeFactory(),
                             sessionRenewer: { _, identity in
            renewAttempts += 1
            if renewAttempts == 1 { throw URLError(.networkConnectionLost) }
            return PairingSession(token: "recovered", expiresAt: Int64.max,
                                  principal: AuthPrincipal(profileId: identity.profileId,
                                                           deviceId: identity.deviceId))
        }, invitationRedeemer: { _, invitation, candidate, _ in
            XCTAssertEqual(candidate.profileId, profile)
            XCTAssertEqual(candidate.profileId, invitation.invite.profileId)
            XCTAssertNil(DeviceIdentity.load(profileId: profile),
                         "candidate must remain in memory until redemption succeeds")
            redeemedDeviceId = candidate.deviceId
        })
        defer { model.signOut() }

        do {
            try await model.pair(invitationText: try invitationText(profile: profile))
            XCTFail("the injected first renewal should fail")
        } catch let error as URLError {
            XCTAssertEqual(error.code, .networkConnectionLost)
        }

        let expectedDeviceId = try XCTUnwrap(redeemedDeviceId)
        XCTAssertEqual(DeviceIdentity.load(profileId: profile)?.deviceId, expectedDeviceId,
                       "successful redemption must install the candidate key")
        XCTAssertEqual(model.storedProfileId, profile)
        XCTAssertEqual(model.peerAddressString, "pair-peer.test:443")

        model.retrySavedIdentity()
        try await waitUntil { model.diagnosticsConfig?.deviceId == expectedDeviceId }
        XCTAssertEqual(renewAttempts, 2)
        XCTAssertEqual(model.diagnosticsConfig?.profileId, profile)
    }

    @MainActor
    func testForegroundRetriesAfterInitialRestoreAuthenticationFailure() async throws {
        let profile = "retry-\(UUID().uuidString)"
        let identity = try identity(profile: profile, seed: 4)
        var renewAttempts = 0
        let model = AppModel(nativeFactory: { _, _, _ in TestTailcatClient() },
                             sessionRenewer: { _, identity in
            renewAttempts += 1
            if renewAttempts == 1 { throw URLError(.cannotConnectToHost) }
            return PairingSession(token: "live", expiresAt: Int64.max,
                                  principal: AuthPrincipal(profileId: identity.profileId,
                                                           deviceId: identity.deviceId))
        }, identityLoader: { $0 == profile ? identity : nil })
        defer { model.signOut() }
        model.storedProfileId = profile
        model.peerAddressString = "peer.test:443"
        model.storedDeviceId = identity.deviceId

        model.restore()
        try await waitUntil { !model.authBusy && model.readinessError != nil }
        XCTAssertEqual(renewAttempts, 1)
        XCTAssertEqual(model.diagnosticsConfig?.peerURL.port, 1,
                       "failed auth leaves the cache-only config installed")

        model.foregrounded()
        try await waitUntil { model.diagnosticsConfig?.peerURL.port == 49_152 }
        XCTAssertEqual(renewAttempts, 2)
    }

    @MainActor
    func testPreloadOpenedBeforeDelayedAuthAttachesOnlyToLiveEndpoint() async throws {
        let profile = "preload-\(UUID().uuidString)"
        let identity = try identity(profile: profile, seed: 5)
        DocDisk.activate(profileId: profile)
        try seedRegistry(profile: profile, device: identity.deviceId,
                         chatId: "preloaded-chat", additionalChatId: "held-chat")
        let auth = AsyncStream<Void>.makeStream()
        let model = AppModel(nativeFactory: { _, _, _ in TestTailcatClient() },
                             sessionRenewer: { _, identity in
            for await _ in auth.stream { break }
            return PairingSession(token: "live", expiresAt: Int64.max,
                                  principal: AuthPrincipal(profileId: identity.profileId,
                                                           deviceId: identity.deviceId))
        }, identityLoader: { $0 == profile ? identity : nil })
        defer { model.signOut() }
        model.storedProfileId = profile
        model.peerAddressString = "peer.test:443"
        model.storedDeviceId = identity.deviceId

        model.restore()
        XCTAssertEqual(Set(model.overviewChats.map(\.id)), ["preloaded-chat", "held-chat"],
                       "the fixture must exercise preloadSessions, not its empty fast path")
        model.preloadSessions()
        let openedPreload = try XCTUnwrap(preloadedStore(in: model, chatId: "preloaded-chat"))
        let heldPreload = try XCTUnwrap(preloadedStore(in: model, chatId: "held-chat"))
        XCTAssertTrue(openedPreload.isDialHeld)
        XCTAssertTrue(heldPreload.isDialHeld)
        XCTAssertNil(openedPreload.attachedPeerURL)
        XCTAssertNil(heldPreload.attachedPeerURL)

        let chat = try XCTUnwrap(model.overviewChats.first { $0.id == "preloaded-chat" })
        let opened = try XCTUnwrap(model.sessionStore(for: chat))
        XCTAssertTrue(opened === openedPreload, "opening must reuse the preloaded cache store")
        XCTAssertFalse(opened.isDialHeld)
        XCTAssertTrue(heldPreload.isDialHeld)

        auth.continuation.yield()
        auth.continuation.finish()
        try await waitUntil {
            opened.attachedPeerURL?.port == 49_152
                && heldPreload.attachedPeerURL?.port == 49_152
        }
        XCTAssertEqual(opened.attachedPeerURL?.port, 49_152)
        XCTAssertEqual(heldPreload.attachedPeerURL?.port, 49_152)
        XCTAssertFalse(opened.isDialHeld)
        XCTAssertTrue(heldPreload.isDialHeld,
                      "an unopened preloaded store must preserve its dial hold after live attach")
    }


    @MainActor
    func testFreshProfileDoesNotImportGlobalOrPreChat2Snapshots() throws {
        let profile = "fresh-\(UUID().uuidString)"
        let chatId = "old-\(UUID().uuidString)"
        let old = LoroDoc()
        let message = try old.getList(id: "messages").pushContainer(child: LoroMap())
        try message.insert(key: "id", v: "legacy-message")
        try message.insert(key: "role", v: "assistant")
        let command = try old.getList(id: "commands").pushContainer(child: LoroMap())
        try command.insert(key: "id", v: "legacy-command")
        try command.insert(key: "status", v: "pending")
        old.commit()
        let snapshot = try old.export(mode: .snapshot)

        let support = FileManager.default.urls(for: .applicationSupportDirectory,
                                               in: .userDomainMask)[0]
        let globalDirectory = support.appendingPathComponent("ZeronDocs", isDirectory: true)
        try FileManager.default.createDirectory(at: globalDirectory, withIntermediateDirectories: true)
        let globalSnapshot = globalDirectory.appendingPathComponent("\(chatId).loro")
        let profileSnapshot = DocDisk.directory(profileId: profile)
            .appendingPathComponent("\(chatId).loro")
        try snapshot.write(to: globalSnapshot, options: .atomic)
        try snapshot.write(to: profileSnapshot, options: .atomic)
        defer {
            try? FileManager.default.removeItem(at: globalSnapshot)
            try? FileManager.default.removeItem(at: profileSnapshot)
            DocDisk.closeProfile()
        }

        DocDisk.activate(profileId: profile)
        let config = AppConfig(peerURL: URL(string: "http://127.0.0.1:1")!,
                               profileId: profile, deviceId: "fresh-device",
                               deviceName: "Test", bearer: "")
        let store = SessionStore(chatId: chatId, config: config)
        let root = store.doc.getDeepValue().mapValue
        XCTAssertTrue(store.entries.isEmpty)
        XCTAssertTrue((root?["messages"]?.listValue ?? []).isEmpty)
        XCTAssertTrue((root?["commands"]?.listValue ?? []).isEmpty,
                      "fresh chat2 state must not carry pending commands from old snapshots")
    }


    private func preloadedStore(in model: AppModel, chatId: String) -> SessionStore? {
        let stores = Mirror(reflecting: model).children
            .first { $0.label == "sessionStores" || $0.label == "_sessionStores" }?.value
            as? [String: SessionStore]
        return stores?[chatId]
    }


    private func pairingNativeFactory() -> AppModel.NativeFactory {
        { _, directory, _ in
            try FileManager.default.createDirectory(at: directory,
                                                    withIntermediateDirectories: true)
            return TestTailcatClient()
        }
    }

    private func invitationText(profile: String) throws -> String {
        let invitation = PairingInvitation(
            version: 1,
            address: "pair-peer.test:443",
            invite: PeerInvite(version: 1, profileId: profile,
                               inviteId: "invite-\(UUID().uuidString.lowercased())",
                               secret: Data(repeating: 0x5a, count: 32).base64URLEncodedString(),
                               expiresAt: Int64.max),
            derpMap: nil)
        return String(decoding: try JSONEncoder().encode(invitation), as: UTF8.self)
    }


    private func identity(profile: String, seed: UInt8) throws -> DeviceIdentity {
        let key = try Curve25519.Signing.PrivateKey(
            rawRepresentation: Data((0..<32).map { seed &+ UInt8($0) }))
        return DeviceIdentity(profileId: profile, privateKey: key)
    }

    private func seedRegistry(profile: String, device: String, chatId: String,
                              additionalChatId: String? = nil) throws {
        let spaceId = "cached-space"
        var rows = [
            RegistryRow(kind: "spaces", id: spaceId, seq: 1, deleted: false, delHlc: nil,
                        fields: ["id": .string(spaceId), "deviceId": .string("host"),
                                 "path": .string("/repo"), "createdAt": .int(1)], clocks: [:]),
            RegistryRow(kind: "chats", id: chatId, seq: 2, deleted: false, delHlc: nil,
                        fields: ["id": .string(chatId), "deviceId": .string("host"),
                                 "title": .string("Cached chat"), "archived": .bool(false),
                                 "createdAt": .int(2), "roomGen": .int(2),
                                 "spaceId": .string(spaceId)], clocks: [:]),
        ]
        if let additionalChatId {
            rows.append(RegistryRow(
                kind: "chats", id: additionalChatId, seq: 3, deleted: false, delHlc: nil,
                fields: ["id": .string(additionalChatId), "deviceId": .string("host"),
                         "title": .string("Held chat"), "archived": .bool(false),
                         "createdAt": .int(3), "roomGen": .int(2),
                         "spaceId": .string(spaceId)], clocks: [:]))
        }
        let doc = RegistryDoc(deviceId: device)
        doc.applyState(seq: UInt64(rows.count), full: true, gcFloor: 0, rows: rows)
        try doc.toData().write(to: DocDisk.registryURL(orgId: profile, userId: profile),
                               options: .atomic)
    }

    private func seedTranscript(chatId: String) throws {
        let doc = LoroDoc()
        let message = try doc.getList(id: "messages").pushContainer(child: LoroMap())
        try message.insert(key: "id", v: "cached-message")
        try message.insert(key: "role", v: "assistant")
        try message.insert(key: "createdAt", v: Int64(1))
        try message.insert(key: "deviceId", v: "host")
        try message.insert(key: "status", v: "complete")
        let parts = try message.insertContainer(key: "parts", child: LoroList())
        let text = try parts.pushContainer(child: LoroMap())
        try text.insert(key: "id", v: "text-1")
        try text.insert(key: "kind", v: "text")
        try text.insert(key: "text", v: "available offline")
        doc.commit()
        DocDisk.saveChat2(doc: doc, id: chatId, cursor: 7, verified: true)
    }

    @MainActor
    private func waitUntil(_ predicate: () -> Bool) async throws {
        for _ in 0..<100 {
            if predicate() { return }
            try await Task.sleep(nanoseconds: 10_000_000)
        }
        XCTFail("condition did not become true")
    }
}

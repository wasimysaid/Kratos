// Profile-bound connection configuration. Every application request goes to
// the embedded Tailcat client's loopback proxy and carries a short-lived bearer
// proven by the device's Ed25519 key.

import Foundation

final class AppConfig: @unchecked Sendable {
    let peerURL: URL
    let profileId: String
    let deviceId: String
    let deviceName: String

    private let lock = NSLock()
    private let identity: DeviceIdentity?
    private var session: PairingSession
    private var renewTask: Task<PairingSession?, Never>?

    private let onRevoked: @Sendable () -> Void

    private let authSession: URLSession

    init(peerURL: URL, profileId: String, deviceId: String, deviceName: String,
         identity: DeviceIdentity?, session: PairingSession,
         authSession: URLSession = .shared,
         onRevoked: @escaping @Sendable () -> Void = {}) {
        self.peerURL = peerURL
        self.profileId = profileId
        self.deviceId = deviceId
        self.deviceName = deviceName
        self.identity = identity
        self.session = session
        self.onRevoked = onRevoked

        self.authSession = authSession
    }

    /// Test/demo initializer. Production always supplies an identity and renews.
    init(peerURL: URL, profileId: String, deviceId: String, deviceName: String,
         bearer: String = "test-bearer") {
        self.peerURL = peerURL
        self.profileId = profileId
        self.deviceId = deviceId
        self.deviceName = deviceName
        identity = nil
        session = PairingSession(token: bearer, expiresAt: Int64.max,
                                 principal: AuthPrincipal(profileId: profileId,
                                                          deviceId: deviceId))

        onRevoked = {}

        authSession = .shared
    }

    func currentToken() async -> String? {
        let current = lock.withLock { session }
        if current.expiresAt > Int64(Date().timeIntervalSince1970) + 60 { return current.token }
        guard let identity else { return nil }
        let task = lock.withLock {
            if let renewTask { return renewTask }
            let task = Task<PairingSession?, Never> { [peerURL, authSession] in
                let refreshed: PairingSession?
                do {
                    refreshed = try await AuthClient(baseURL: peerURL, session: authSession)
                        .renew(identity: identity)
                } catch PairingError.revoked {
                    refreshed = nil
                    self.onRevoked()
                } catch {
                    refreshed = nil
                }
                self.lock.withLock {
                    if let refreshed { self.session = refreshed }
                    self.renewTask = nil
                }
                return refreshed
            }
            renewTask = task
            return task
        }
        return await task.value?.token

    }

    func reportUnauthorized() {
        onRevoked()
    }

    private var wsBase: URL {
        var components = URLComponents(url: peerURL, resolvingAgainstBaseURL: false)!
        components.scheme = components.scheme == "http" ? "ws" : "wss"
        return components.url!
    }

    func registrySocketURL() async -> URL? {
        guard let token = await currentToken() else { return nil }
        var url = wsBase.appending(path: "registry/\(profileId)/ws")
        url.append(queryItems: [.init(name: "token", value: token),
                                .init(name: "device", value: deviceId)])
        return url
    }

    func chat2SocketURL(chatId: String) async -> URL? {
        guard let token = await currentToken() else { return nil }
        var url = wsBase.appending(path: "chat2/\(chatId)/ws")
        url.append(queryItems: [.init(name: "token", value: token),
                                .init(name: "device", value: deviceId)])
        return url
    }

    func chat2CheckpointRequest(chatId: String) async -> URLRequest? {
        authorized(peerURL.appending(path: "chat2/\(chatId)/checkpoint"),
                   token: await currentToken())
    }

    func chat2RowsRequest(chatId: String, after: UInt64) async -> URLRequest? {
        var url = peerURL.appending(path: "chat2/\(chatId)/rows")
        url.append(queryItems: [.init(name: "after", value: String(after)),
                                .init(name: "device", value: deviceId)])
        return authorized(url, token: await currentToken())
    }

    func chat2PushRequest(chatId: String, batchId: String) async -> URLRequest? {
        var url = peerURL.appending(path: "chat2/\(chatId)/rows")
        url.append(queryItems: [.init(name: "batchId", value: batchId),
                                .init(name: "device", value: deviceId)])
        guard var request = authorized(url, token: await currentToken()) else { return nil }
        request.httpMethod = "POST"
        return request
    }

    func registryRowsRequest(since: UInt64?) async -> URLRequest? {
        var url = peerURL.appending(path: "registry/\(profileId)/rows")
        var items = [URLQueryItem(name: "device", value: deviceId),
                     .init(name: "beat", value: "1")]
        if let since { items.append(.init(name: "since", value: String(since))) }
        url.append(queryItems: items)
        return authorized(url, token: await currentToken())
    }

    func registryPushRequest() async -> URLRequest? {
        var url = peerURL.appending(path: "registry/\(profileId)/push")
        url.append(queryItems: [.init(name: "device", value: deviceId)])
        guard var request = authorized(url, token: await currentToken()) else { return nil }
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        return request
    }

    func deviceStatus(deviceId: String) async -> String {
        guard let request = authorized(peerURL.appending(path: "device/\(deviceId)/status"),
                                       token: await currentToken()),
              let (data, response) = try? await URLSession.shared.data(for: request),
              let http = response as? HTTPURLResponse else { return "unreachable" }
        return "http=\(http.statusCode) body=\(String(data: data, encoding: .utf8) ?? "")"
    }

    func nudge(deviceId: String, chatId: String) async {
        guard var request = authorized(peerURL.appending(path: "device/\(deviceId)/nudge"),
                                       token: await currentToken()) else { return }
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try? JSONSerialization.data(withJSONObject: ["chatId": chatId])
        _ = try? await URLSession.shared.data(for: request)
    }

    private func authorized(_ url: URL, token: String?) -> URLRequest? {
        guard let token else { return nil }
        var request = URLRequest(url: url)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }
}

// Profile/device pairing and short-lived bearer renewal. Proofs match
// crates/engine/src/peer_auth.rs exactly: domain separator followed by u32
// big-endian length-prefixed fields. Invitation routing fields never establish
// identity; the authenticated device id is derived from the Ed25519 public key.

import CryptoKit
import Foundation
import Security

struct PeerInvite: Codable, Equatable {
    let version: UInt8
    let profileId: String
    let inviteId: String
    let secret: String
    let expiresAt: Int64
}

struct PairingInvitation: Codable, Equatable {
    let version: Int
    let address: String
    let invite: PeerInvite
    let derpMap: String?

    static func parse(_ input: String) throws -> Self {
        let trimmed = input.trimmingCharacters(in: .whitespacesAndNewlines)
        let code: String
        if let url = URL(string: trimmed), url.scheme == "kratos",
           let value = URLComponents(url: url, resolvingAgainstBaseURL: false)?
            .queryItems?.first(where: { $0.name == "payload" })?.value {
            code = value
        } else {
            code = trimmed
        }

        let data: Data?
        if code.hasPrefix("{") {
            data = Data(code.utf8)
        } else if code.hasPrefix("kratos-pair:") {
            data = Data(base64URL: String(code.dropFirst("kratos-pair:".count)))
        } else {
            // QR/deep-link producers may carry the encoded JSON as the payload.
            data = Data(base64URL: code)
        }
        guard let data,
              data.count <= 16 * 1024,
              let invitation = try? JSONDecoder().decode(Self.self, from: data),
              invitation.version == 1,
              invitation.invite.version == 1,
              !invitation.address.isEmpty,
              !invitation.invite.profileId.isEmpty,
              !invitation.invite.inviteId.isEmpty,
              Data(base64URL: invitation.invite.secret)?.count == 32
        else { throw PairingError.invalidInvitation }
        return invitation
    }
}

struct PairingChallenge: Codable, Equatable {
    let profileId: String
    let deviceId: String
    let challengeId: String
    let nonce: String
    let expiresAt: Int64
}

struct AuthPrincipal: Codable, Equatable {
    let profileId: String
    let deviceId: String
}

struct PairingSession: Codable, Equatable {
    let token: String
    let expiresAt: Int64
    let principal: AuthPrincipal

    var profileId: String { principal.profileId }
    var deviceId: String { principal.deviceId }
}

struct RedeemResponse: Codable, Equatable {
    let profileId: String
    let deviceId: String
}

struct DeviceIdentity: Equatable {
    private static let redeemDomain = Data("kratos.peer-auth.invite-redeem.v1\0".utf8)
    private static let authDomain = Data("kratos.peer-auth.challenge.v1\0".utf8)

    let profileId: String
    let privateKey: Curve25519.Signing.PrivateKey

    var publicKeyData: Data { privateKey.publicKey.rawRepresentation }
    var publicKey: String { publicKeyData.base64URLEncodedString() }
    var deviceId: String {
        "dev_" + Data(SHA256.hash(data: publicKeyData)).base64URLEncodedString()
    }

    static func == (lhs: Self, rhs: Self) -> Bool {
        lhs.profileId == rhs.profileId && lhs.publicKeyData == rhs.publicKeyData
    }

    func redeemSignature(invite: PeerInvite) throws -> String {
        guard invite.profileId == profileId,
              let secret = Data(base64URL: invite.secret), secret.count == 32 else {
            throw PairingError.invalidInvitation
        }
        let payload = Self.framed(domain: Self.redeemDomain, fields: [
            Data([invite.version]), Data(invite.profileId.utf8), Data(invite.inviteId.utf8),
            secret, publicKeyData,
        ])
        return try privateKey.signature(for: payload).base64URLEncodedString()
    }

    func challengeSignature(_ challenge: PairingChallenge) throws -> String {
        guard challenge.profileId == profileId, challenge.deviceId == deviceId,
              let nonce = Data(base64URL: challenge.nonce), nonce.count == 32 else {
            throw PairingError.invalidChallenge
        }
        let payload = Self.framed(domain: Self.authDomain, fields: [
            Data(challenge.profileId.utf8), Data(challenge.deviceId.utf8),
            Data(challenge.challengeId.utf8), nonce,
        ])
        return try privateKey.signature(for: payload).base64URLEncodedString()
    }

    static func createPending() -> Self {
        Self(profileId: "", privateKey: .init())
    }

    func bound(profileId: String) throws -> Self {
        guard !profileId.isEmpty else { throw PairingError.invalidResponse }
        return Self(profileId: profileId, privateKey: privateKey)
    }

    static func load(profileId: String) -> Self? {
        guard let data = Keychain.loadData(key: key(profileId)),
              let privateKey = try? Curve25519.Signing.PrivateKey(rawRepresentation: data)
        else { return nil }
        return Self(profileId: profileId, privateKey: privateKey)
    }

    func persist() throws -> Self {
        guard !profileId.isEmpty else { throw PairingError.invalidResponse }
        try Keychain.saveData(privateKey.rawRepresentation, key: Self.key(profileId))
        return self
    }

    static func delete(profileId: String) { Keychain.delete(key: key(profileId)) }
    private static func key(_ profileId: String) -> String { "device-key:\(profileId)" }

    static func framed(domain: Data, fields: [Data]) -> Data {
        var output = domain
        for field in fields {
            var size = UInt32(field.count).bigEndian
            withUnsafeBytes(of: &size) { output.append(contentsOf: $0) }
            output.append(field)
        }
        return output
    }
}

enum PairingError: LocalizedError {
    case invalidInvitation
    case invalidChallenge
    case invalidResponse

    case revoked
    case http(Int, String)

    var errorDescription: String? {
        switch self {
        case .invalidInvitation: return "This pairing invitation is invalid."
        case .invalidChallenge: return "The peer sent an invalid signing challenge."
        case .invalidResponse: return "The peer returned an invalid pairing response."

        case .revoked: return "This device was revoked. Pair it again to continue."
        case .http(let status, let body): return "Pairing failed (\(status)): \(body)"
        }
    }
}

struct AuthClient: Sendable {
    let baseURL: URL
    var session: URLSession = .shared

    func redeem(invitation: PairingInvitation, identity: DeviceIdentity,
                deviceName: String) async throws {
        let invite = invitation.invite
        guard identity.profileId == invite.profileId else { throw PairingError.invalidResponse }
        let redeemed: RedeemResponse = try await post("pair/redeem", body: [
            "version": invite.version,
            "profileId": invite.profileId,
            "inviteId": invite.inviteId,
            "secret": invite.secret,
            "publicKey": identity.publicKey,
            "signature": try identity.redeemSignature(invite: invite),
            "displayName": deviceName,
        ])
        guard redeemed.profileId == invite.profileId,
              redeemed.deviceId == identity.deviceId else { throw PairingError.invalidResponse }
    }

    func renew(identity: DeviceIdentity) async throws -> PairingSession {
        try await authenticate(identity: identity)
    }

    private func authenticate(identity: DeviceIdentity) async throws -> PairingSession {
        let challenge: PairingChallenge = try await post("pair/challenge", body: [
            "profileId": identity.profileId,
            "deviceId": identity.deviceId,
        ])
        let proof: PairingSession = try await post("pair/authenticate", body: [
            "profileId": challenge.profileId,
            "deviceId": challenge.deviceId,
            "challengeId": challenge.challengeId,
            "nonce": challenge.nonce,
            "signature": try identity.challengeSignature(challenge),
        ])
        guard proof.profileId == identity.profileId,
              proof.deviceId == identity.deviceId,
              !proof.token.isEmpty else { throw PairingError.invalidResponse }
        return proof
    }

    private func post<T: Decodable>(_ path: String, body: [String: Any]) async throws -> T {
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: body, options: [.sortedKeys])
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse else { throw PairingError.invalidResponse }

        if http.statusCode == 401 || http.statusCode == 403 { throw PairingError.revoked }
        guard (200..<300).contains(http.statusCode) else {
            throw PairingError.http(http.statusCode, String(data: data, encoding: .utf8) ?? "")
        }
        return try JSONDecoder().decode(T.self, from: data)
    }
}

enum Keychain {
    private static let service = "sh.kratos.ios"

    static func saveData(_ data: Data, key: String) throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
        ]
        SecItemDelete(query as CFDictionary)
        var add = query
        add[kSecValueData as String] = data
        add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        let status = SecItemAdd(add as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw NSError(domain: NSOSStatusErrorDomain, code: Int(status))
        }
    }

    static func loadData(key: String) -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess else { return nil }
        return result as? Data
    }

    static func delete(key: String) {
        SecItemDelete([
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: key,
        ] as CFDictionary)
    }
}

extension Data {
    init?(base64URL value: String) {
        var base64 = value.replacingOccurrences(of: "-", with: "+")
            .replacingOccurrences(of: "_", with: "/")
        while base64.count % 4 != 0 { base64 += "=" }
        self.init(base64Encoded: base64)
    }

    func base64URLEncodedString() -> String {
        base64EncodedString().replacingOccurrences(of: "+", with: "-")
            .replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
    }
}

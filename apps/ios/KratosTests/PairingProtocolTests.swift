import CryptoKit
import XCTest
@testable import Kratos

final class PairingProtocolTests: XCTestCase {
    private let profile = "11111111-1111-4111-8111-111111111111"
    private let inviteId = "22222222-2222-4222-8222-222222222222"
    private let challengeId = "33333333-3333-4333-8333-333333333333"

    func testInvitationParsesRuntimeCodeRawJSONAndDeepLink() throws {
        let secret = Data((0..<32).map(UInt8.init)).base64URLEncodedString()
        let json = #"{"version":1,"address":"tcpeer","invite":{"version":1,"profileId":"11111111-1111-4111-8111-111111111111","inviteId":"22222222-2222-4222-8222-222222222222","secret":"\#(secret)","expiresAt":2000000000},"derpMap":"https://peer/derp.json"}"#
        let encoded = Data(json.utf8).base64URLEncodedString()
        let compact = "kratos-pair:\(encoded)"
        let raw = try PairingInvitation.parse(json)
        let pasted = try PairingInvitation.parse(compact)
        let linked = try PairingInvitation.parse("kratos://pair?payload=\(compact)")
        XCTAssertEqual(raw, pasted)
        XCTAssertEqual(raw, linked)
        XCTAssertEqual(raw.invite.profileId, profile)
        XCTAssertEqual(raw.address, "tcpeer")
    }

    func testInvitationRejectsUnsupportedOrMalformedEnvelope() {
        let empty = Data(#"{"version":2,"address":"tcpeer","invite":{}}"#.utf8)
            .base64URLEncodedString()
        XCTAssertThrowsError(try PairingInvitation.parse(empty))
        XCTAssertThrowsError(try PairingInvitation.parse("***"))
    }

    func testRedeemProofMatchesServerFraming() throws {
        let key = try Curve25519.Signing.PrivateKey(rawRepresentation: Data((0..<32).map(UInt8.init)))
        let identity = DeviceIdentity(profileId: profile, privateKey: key)
        let secret = Data((32..<64).map(UInt8.init))
        let invite = PeerInvite(version: 1, profileId: profile, inviteId: inviteId,
                                secret: secret.base64URLEncodedString(), expiresAt: 2_000_000_000)
        let signature = try XCTUnwrap(Data(base64URL: identity.redeemSignature(invite: invite)))
        let payload = framed(domain: "kratos.peer-auth.invite-redeem.v1\0", fields: [
            Data([1]), Data(profile.utf8), Data(inviteId.utf8), secret,
            key.publicKey.rawRepresentation,
        ])
        XCTAssertTrue(key.publicKey.isValidSignature(signature, for: payload))
        XCTAssertFalse(key.publicKey.isValidSignature(signature, for: Data(invite.secret.utf8)))

        let wrongProfile = DeviceIdentity(profileId: "44444444-4444-4444-8444-444444444444",
                                          privateKey: key)
        XCTAssertThrowsError(try wrongProfile.redeemSignature(invite: invite))
    }

    func testIdentityEqualityUsesProfileAndPublicKey() throws {
        let rawKey = Data((0..<32).map(UInt8.init))
        let sameKey = try Curve25519.Signing.PrivateKey(rawRepresentation: rawKey)
        let reloadedKey = try Curve25519.Signing.PrivateKey(rawRepresentation: rawKey)
        let differentKey = try Curve25519.Signing.PrivateKey(
            rawRepresentation: Data((1...32).map(UInt8.init)))
        let identity = DeviceIdentity(profileId: profile, privateKey: sameKey)

        XCTAssertEqual(identity, DeviceIdentity(profileId: profile, privateKey: reloadedKey))
        XCTAssertNotEqual(identity, DeviceIdentity(profileId: "other-profile", privateKey: reloadedKey))
        XCTAssertNotEqual(identity, DeviceIdentity(profileId: profile, privateKey: differentKey))
    }

    func testChallengeProofAndDerivedDeviceIdMatchServerContract() throws {
        let key = try Curve25519.Signing.PrivateKey(rawRepresentation: Data((0..<32).map(UInt8.init)))
        let identity = DeviceIdentity(profileId: profile, privateKey: key)
        let expectedDevice = "dev_" + Data(SHA256.hash(data: key.publicKey.rawRepresentation))
            .base64URLEncodedString()
        XCTAssertEqual(identity.deviceId, expectedDevice)
        let nonce = Data((64..<96).map(UInt8.init))
        let challenge = PairingChallenge(profileId: profile, deviceId: expectedDevice,
                                         challengeId: challengeId,
                                         nonce: nonce.base64URLEncodedString(),
                                         expiresAt: 2_000_000_000)
        let signature = try XCTUnwrap(Data(base64URL: identity.challengeSignature(challenge)))
        let payload = framed(domain: "kratos.peer-auth.challenge.v1\0", fields: [
            Data(profile.utf8), Data(expectedDevice.utf8), Data(challengeId.utf8), nonce,
        ])
        XCTAssertTrue(key.publicKey.isValidSignature(signature, for: payload))
    }

    func testChallengeIdentityMismatchAndMalformedNonceFailClosed() throws {
        let identity = DeviceIdentity(profileId: profile,
                                      privateKey: try .init(rawRepresentation: Data((0..<32).map(UInt8.init))))
        let wrong = PairingChallenge(profileId: profile, deviceId: "dev_wrong",
                                     challengeId: challengeId,
                                     nonce: Data((0..<32).map(UInt8.init)).base64URLEncodedString(),
                                     expiresAt: 0)
        XCTAssertThrowsError(try identity.challengeSignature(wrong))
        let malformed = PairingChallenge(profileId: profile, deviceId: identity.deviceId,
                                         challengeId: challengeId, nonce: "AA", expiresAt: 0)
        XCTAssertThrowsError(try identity.challengeSignature(malformed))
    }

    func testProfileDirectoriesAreDistinctAndDoNotUseRawProfileIds() {
        let a = DocDisk.directory(profileId: "account/shared")
        let b = DocDisk.directory(profileId: "account:shared")
        XCTAssertNotEqual(a, b)
        XCTAssertNotEqual(a.lastPathComponent, "account/shared")
        XCTAssertNotEqual(b.lastPathComponent, "account:shared")
        XCTAssertEqual(a.deletingLastPathComponent(), b.deletingLastPathComponent())
    }

    private func framed(domain: String, fields: [Data]) -> Data {
        var output = Data(domain.utf8)
        for field in fields {
            var count = UInt32(field.count).bigEndian
            withUnsafeBytes(of: &count) { output.append(contentsOf: $0) }
            output.append(field)
        }
        return output
    }
}

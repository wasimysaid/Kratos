import CryptoKit
import XCTest
@testable import Kratos

private final class PeerURLProtocol: URLProtocol {
    nonisolated(unsafe) static var handler: ((URLRequest) throws -> (Int, Data))?

    override class func canInit(with request: URLRequest) -> Bool { true }
    override class func canonicalRequest(for request: URLRequest) -> URLRequest { request }

    override func startLoading() {
        do {
            let (status, data) = try Self.handler?(request) ?? (500, Data())
            let response = HTTPURLResponse(url: request.url!, statusCode: status,
                                           httpVersion: "HTTP/1.1",
                                           headerFields: ["Content-Type": "application/json"])!
            client?.urlProtocol(self, didReceive: response, cacheStoragePolicy: .notAllowed)
            client?.urlProtocol(self, didLoad: data)
            client?.urlProtocolDidFinishLoading(self)
        } catch {
            client?.urlProtocol(self, didFailWithError: error)
        }
    }

    override func stopLoading() {}

    static func body(of request: URLRequest, limit: Int = 1024 * 1024) throws -> Data? {
        if let body = request.httpBody {
            guard body.count <= limit else { throw URLError(.dataLengthExceedsMaximum) }
            return body
        }
        guard let stream = request.httpBodyStream else { return nil }
        stream.open()
        defer { stream.close() }
        var body = Data()
        var buffer = [UInt8](repeating: 0, count: 16 * 1024)
        while true {
            let count = stream.read(&buffer, maxLength: buffer.count)
            if count < 0 { throw stream.streamError ?? URLError(.cannotDecodeRawData) }
            if count == 0 { return body }
            guard count <= limit - body.count else { throw URLError(.dataLengthExceedsMaximum) }
            body.append(contentsOf: buffer[..<count])
        }
    }


    static func session() -> URLSession {
        let configuration = URLSessionConfiguration.ephemeral
        configuration.protocolClasses = [PeerURLProtocol.self]
        return URLSession(configuration: configuration)
    }
}

final class PeerIntegrationTests: XCTestCase {
    override func tearDown() {
        PeerURLProtocol.handler = nil
        super.tearDown()
    }

    func testCustodyUploadResumesAtServerOffsetAndCommits() async throws {
        let bytes = Data((0..<10).map(UInt8.init))
        let expectedDigest = SHA256.hash(data: bytes).map { String(format: "%02x", $0) }.joined()
        var calls: [URLRequest] = []
        PeerURLProtocol.handler = { request in
            calls.append(request)
            XCTAssertEqual(request.value(forHTTPHeaderField: "Authorization"), "Bearer token")
            switch calls.count {
            case 1:
                XCTAssertEqual(request.httpMethod, "POST")
                XCTAssertEqual(request.url?.path, "/attachment/upload-1")
                let body = try XCTUnwrap(try PeerURLProtocol.body(of: request))
                let json = try XCTUnwrap(JSONSerialization.jsonObject(with: body) as? [String: Any])
                XCTAssertEqual(json["targetDevice"] as? String, "dev_host")
                XCTAssertEqual(json["fileName"] as? String, "photo.png")
                XCTAssertEqual(json["length"] as? Int, bytes.count)
                XCTAssertEqual(json["digest"] as? String, expectedDigest)
                return (200, try self.meta(committed: false, nextOffset: 4, length: bytes.count))
            case 2:
                XCTAssertEqual(request.httpMethod, "PUT")
                XCTAssertEqual(request.url?.path, "/attachment/upload-1/chunk")
                let query = URLComponents(url: request.url!, resolvingAgainstBaseURL: false)?.queryItems
                XCTAssertEqual(query?.first { $0.name == "targetDevice" }?.value, "dev_host")
                XCTAssertEqual(query?.first { $0.name == "offset" }?.value, "4")
                let body = try XCTUnwrap(try PeerURLProtocol.body(of: request))
                XCTAssertEqual(body, bytes.subdata(in: 4..<bytes.count))
                return (200, try self.meta(committed: false, nextOffset: 10, length: bytes.count))
            case 3:
                XCTAssertEqual(request.httpMethod, "POST")
                XCTAssertEqual(request.url?.path, "/attachment/upload-1/commit")
                return (200, try self.meta(committed: true, nextOffset: 10, length: bytes.count))
            default:
                XCTFail("unexpected custody request")
                return (500, Data())
            }
        }
        DocDisk.activate(profileId: "custody-test")
        UploadStash.save(uploadId: "upload-1", data: bytes)
        let config = AppConfig(peerURL: URL(string: "http://peer.test")!,
                               profileId: "profile", deviceId: "dev_sender",
                               deviceName: "phone", bearer: "token")
        try await CustodyUploader.upload(config: config, targetDevice: "dev_host",
                                         uploadId: "upload-1", name: "photo.png",
                                         data: bytes, session: PeerURLProtocol.session())
        XCTAssertEqual(calls.count, 3)
        XCTAssertNil(UploadStash.load(uploadId: "upload-1"))
    }

    func testUnauthorizedRenewalReportsRevocationOnce() async throws {
        PeerURLProtocol.handler = { _ in (401, Data(#"{"error":"unauthorized"}"#.utf8)) }
        let key = try Curve25519.Signing.PrivateKey(rawRepresentation: Data((0..<32).map(UInt8.init)))
        let identity = DeviceIdentity(profileId: "11111111-1111-4111-8111-111111111111",
                                      privateKey: key)
        let expired = PairingSession(token: "expired", expiresAt: 0,
                                     principal: AuthPrincipal(profileId: identity.profileId,
                                                              deviceId: identity.deviceId))
        let revoked = expectation(description: "revocation callback")
        revoked.expectedFulfillmentCount = 1
        let config = AppConfig(peerURL: URL(string: "http://peer.test")!,
                               profileId: identity.profileId, deviceId: identity.deviceId,
                               deviceName: "phone", identity: identity, session: expired,
                               authSession: PeerURLProtocol.session()) {
            revoked.fulfill()
        }
        let token = await config.currentToken()
        XCTAssertNil(token)
        await fulfillment(of: [revoked], timeout: 1)
    }

    func testDelayedAuthCompletionIsRejectedAfterNewAttemptOrCancel() {
        var gate = AuthTransitionGate()
        let first = gate.begin()
        let second = gate.begin()
        XCTAssertFalse(gate.accepts(first))
        XCTAssertTrue(gate.accepts(second))
        gate.cancel()
        XCTAssertFalse(gate.accepts(second))
    }

    private func meta(committed: Bool, nextOffset: Int, length: Int) throws -> Data {
        try JSONSerialization.data(withJSONObject: [
            "senderDevice": "dev_sender", "targetDevice": "dev_host",
            "fileName": "photo.png", "length": length,
            "digest": String(repeating: "0", count: 64),
            "committed": committed, "nextOffset": nextOffset,
        ])
    }
}

import CryptoKit
import Foundation

struct CustodyUploadMeta: Codable, Equatable {
    let senderDevice: String
    let targetDevice: String
    let fileName: String
    let length: UInt64
    let digest: String
    let committed: Bool
    let nextOffset: UInt64
}

enum CustodyUploadError: LocalizedError {
    case unauthorized
    case rejected(Int, String)
    case invalidResponse

    var errorDescription: String? {
        switch self {
        case .unauthorized: "This device was revoked. Pair it again to continue."
        case .rejected(let status, let body): "Attachment custody failed (\(status)): \(body)"
        case .invalidResponse: "The peer returned an invalid attachment custody response."
        }
    }
}

enum CustodyUploader {
    static let chunkBytes = 512 * 1024

    static func upload(config: AppConfig, targetDevice: String, uploadId: String,
                       name: String, data: Data, session: URLSession = .shared,
                       progress: (@MainActor @Sendable (Double) -> Void)? = nil) async throws {
        let digest = SHA256.hash(data: data).map { String(format: "%02x", $0) }.joined()
        var initRequest = try await request(config: config, path: "attachment/\(uploadId)")
        initRequest.httpMethod = "POST"
        initRequest.setValue("application/json", forHTTPHeaderField: "Content-Type")
        initRequest.httpBody = try JSONSerialization.data(withJSONObject: [
            "targetDevice": targetDevice, "fileName": name,
            "length": data.count, "digest": digest,
        ])
        var meta = try await send(initRequest, config: config, session: session)
        var offset = min(Int(meta.nextOffset), data.count)
        while offset < data.count {
            let end = min(offset + chunkBytes, data.count)
            var url = config.peerURL.appending(path: "attachment/\(uploadId)/chunk")
            url.append(queryItems: [.init(name: "targetDevice", value: targetDevice),
                                    .init(name: "offset", value: String(offset))])
            var chunk = try await request(config: config, url: url)
            chunk.httpMethod = "PUT"
            chunk.setValue("application/octet-stream", forHTTPHeaderField: "Content-Type")
            chunk.httpBody = data.subdata(in: offset..<end)
            meta = try await send(chunk, config: config, session: session)
            guard meta.nextOffset > UInt64(offset) else { throw CustodyUploadError.invalidResponse }
            offset = min(Int(meta.nextOffset), data.count)
            UploadStash.saveProgress(uploadId: uploadId, nextOffset: offset)
            if let progress { await progress(min(Double(offset) / Double(max(data.count, 1)), 0.99)) }
        }
        var commitURL = config.peerURL.appending(path: "attachment/\(uploadId)/commit")
        commitURL.append(queryItems: [.init(name: "targetDevice", value: targetDevice)])
        var commit = try await request(config: config, url: commitURL)
        commit.httpMethod = "POST"
        meta = try await send(commit, config: config, session: session)
        guard meta.committed, meta.nextOffset == UInt64(data.count) else {
            throw CustodyUploadError.invalidResponse
        }
        UploadStash.delete(uploadId: uploadId)
    }

    private static func request(config: AppConfig, path: String) async throws -> URLRequest {
        try await request(config: config, url: config.peerURL.appending(path: path))
    }

    private static func request(config: AppConfig, url: URL) async throws -> URLRequest {
        guard let token = await config.currentToken() else { throw CustodyUploadError.unauthorized }
        var request = URLRequest(url: url)
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        return request
    }

    private static func send(_ request: URLRequest, config: AppConfig,
                             session: URLSession) async throws -> CustodyUploadMeta {
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse else { throw CustodyUploadError.invalidResponse }
        if http.statusCode == 401 || http.statusCode == 403 {
            config.reportUnauthorized()
            throw CustodyUploadError.unauthorized
        }
        guard (200..<300).contains(http.statusCode) else {
            throw CustodyUploadError.rejected(http.statusCode, String(data: data, encoding: .utf8) ?? "")
        }
        return try JSONDecoder().decode(CustodyUploadMeta.self, from: data)
    }
}

// Simulator diagnostics. Demo mode remains offline; live diagnostics reuse an
// already paired profile and never bypass device-key authentication.

import Foundation

@MainActor
enum E2ERunner {
    static var logURL: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("e2e.log")
    }

    static func log(_ line: String) {
        let stamped = "[\(Int(Date().timeIntervalSince1970))] \(line)\n"
        print("E2E: \(line)")
        if let handle = try? FileHandle(forWritingTo: logURL) {
            handle.seekToEndOfFile(); handle.write(Data(stamped.utf8)); try? handle.close()
        } else { try? Data(stamped.utf8).write(to: logURL) }
    }

    static func run(model: AppModel) async {
        try? FileManager.default.removeItem(at: logURL)
        model.enterDemoMode()
        log("OK offline demo loaded: spaces=\(model.spaces.count), chats=\(model.overviewChats.count)")
    }

    static func runLive(model: AppModel) async {
        try? FileManager.default.removeItem(at: logURL)
        log("paired profile=\(model.storedProfileId.prefix(18)) peer=\(model.peerAddressString.prefix(24))")
        guard let workspace = model.workspace, let config = model.diagnosticsConfig else {
            log("FAIL no paired workspace")
            return
        }
        log("workspace connected=\(workspace.connected), devices=\(workspace.devices.count)")
        for device in workspace.devices where device.platform != "ios" {
            log("\(device.name) /status → \(await config.deviceStatus(deviceId: device.id))")
        }
    }
}

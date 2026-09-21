// Native secure pairing. Invitations may be pasted or opened as
// `kratos://pair?payload=…` links (the same payload carried by a QR code).

import SwiftUI
import UIKit

struct SignInView: View {
    @Environment(AppModel.self) private var model
    @State private var invitation = ""
    @State private var busy = false
    @State private var error: String?

    var body: some View {
        ZStack {
            Theme.bg.ignoresSafeArea()
            VStack(spacing: 28) {
                Spacer()
                VStack(spacing: 20) {
                    KratosMark().frame(width: 72, height: 72)
                    VStack(spacing: 6) {
                        Text("Pair this device")
                            .font(Theme.sans(26, weight: .semibold))
                            .foregroundStyle(Theme.text)
                        Text("Paste an invitation from a trusted Kratos device, or scan its QR link.")
                            .font(Theme.sans(14))
                            .foregroundStyle(Theme.textMuted)
                            .multilineTextAlignment(.center)
                    }
                }
                VStack(spacing: 12) {
                    TextField("kratos://pair?payload=…", text: $invitation, axis: .vertical)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .font(Theme.mono(13))
                        .foregroundStyle(Theme.text)
                        .padding(14)
                        .background(Theme.surface, in: RoundedRectangle(cornerRadius: 14))
                    HStack(spacing: 10) {
                        Button("Paste") {
                            invitation = UIPasteboard.general.string ?? ""
                        }
                        .buttonStyle(.bordered)
                        Button {
                            pair()
                        } label: {
                            Group {
                                if busy { ProgressView().tint(Theme.bg) }
                                else { Text("Pair securely") }
                            }
                            .font(Theme.sans(15, weight: .semibold))
                            .foregroundStyle(Theme.bg)
                            .frame(maxWidth: .infinity).frame(height: 48)
                            .background(Theme.text, in: RoundedRectangle(cornerRadius: 14))
                        }
                        .buttonStyle(.plain)
                        .disabled(busy || model.authBusy || invitation.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    }
                    if let message = error ?? model.readinessError {
                        Text(message).font(Theme.sans(13)).foregroundStyle(Theme.danger)
                            .multilineTextAlignment(.center)
                    }
                    if !model.storedProfileId.isEmpty {
                        Button("Retry saved device") { model.retrySavedIdentity() }
                            .disabled(model.authBusy)
                            .font(Theme.sans(13)).foregroundStyle(Theme.textMuted)
                    }
                    Button("Explore offline demo") { model.enterDemoMode() }
                        .disabled(busy || model.authBusy)
                        .font(Theme.sans(13)).foregroundStyle(Theme.textMuted)
                }
                Spacer()
            }
            .padding(.horizontal, 28)
            .frame(maxWidth: 520)
        }
        .onOpenURL { url in
            guard url.scheme == "kratos" else { return }
            invitation = url.absoluteString
            pair()
        }
    }

    private func pair() {
        guard !busy else { return }
        busy = true
        error = nil
        Task {
            do { try await model.pair(invitationText: invitation) }
            catch { self.error = error.localizedDescription }
            busy = false
        }
    }
}

struct KratosMark: View {
    var color: Color = Theme.text
    static let cells: [(CGFloat, CGFloat)] = [
        (0, 600), (0, 720), (240, 840), (240, 720), (120, 840), (120, 600),
        (240, 600), (0, 480), (0, 360), (480, 840), (480, 720), (120, 360),
        (120, 240), (240, 360), (600, 720), (480, 600), (360, 360), (240, 240),
        (600, 600), (720, 600), (720, 480), (240, 120), (600, 380), (720, 240),
        (720, 0), (480, 240), (480, 0), (120, 480), (240, 480), (360, 840),
        (360, 720), (360, 600), (360, 480), (120, 720),
    ]
    var body: some View {
        KratosMarkShape().fill(color).aspectRatio(820 / 940, contentMode: .fit)
    }
}

struct KratosMarkShape: Shape {
    func path(in rect: CGRect) -> Path {
        var path = Path()
        let scale = min(rect.width / 820, rect.height / 940)
        let dx = rect.minX + (rect.width - 820 * scale) / 2
        let dy = rect.minY + (rect.height - 940 * scale) / 2
        for (x, y) in KratosMark.cells {
            path.addRoundedRect(
                in: CGRect(x: dx + x * scale, y: dy + y * scale,
                           width: 100 * scale, height: 100 * scale),
                cornerSize: CGSize(width: 16 * scale, height: 16 * scale)
            )
        }
        return path
    }
}

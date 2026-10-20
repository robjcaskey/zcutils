import Foundation
import SystemExtensions
final class Activation: NSObject, OSSystemExtensionRequestDelegate {
    func request(_ request: OSSystemExtensionRequest, actionForReplacingExtension existing: OSSystemExtensionProperties, withExtension ext: OSSystemExtensionProperties) -> OSSystemExtensionRequest.ReplacementAction { .replace }
    func requestNeedsUserApproval(_ request: OSSystemExtensionRequest) { print("Approve zcutils in System Settings > Privacy & Security.") }
    func request(_ request: OSSystemExtensionRequest, didFinishWithResult result: OSSystemExtensionRequest.Result) { print("Extension request completed: \(result.rawValue)"); exit(0) }
    func request(_ request: OSSystemExtensionRequest, didFailWithError error: Error) { fputs("Extension activation failed: \(error)\n", stderr); exit(1) }
}
let delegate = Activation()
let request = CommandLine.arguments.contains("--deactivate")
    ? OSSystemExtensionRequest.deactivationRequest(forExtensionWithIdentifier: "org.zcutils.storage", queue: .main)
    : OSSystemExtensionRequest.activationRequest(forExtensionWithIdentifier: "org.zcutils.storage", queue: .main)
request.delegate = delegate
OSSystemExtensionManager.shared.submitRequest(request)
RunLoop.main.run()

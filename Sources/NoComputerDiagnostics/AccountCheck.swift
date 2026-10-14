import Foundation
import OpenDirectory

// Checks a viewer's login against the account this app runs as: the signed-in
// user whose screen is shared. No other account is looked up, so a login can't
// probe which accounts exist. Blocking; call it off the main thread. Never logs
// the username or password.
enum AccountCheck {
    static func verify(username: String, password: LocalLink.Password) -> Bool {
        guard names(username) else { return false }
        do {
            let node = try ODNode(session: ODSession.default(), type: ODNodeType(kODNodeTypeAuthentication))
            let record = try node.record(withRecordType: kODRecordTypeUsers, name: NSUserName(),
                                         attributes: [kODAttributeTypeUniqueID])
            // The record found by name must be this process's own account.
            let ids = try record.values(forAttribute: kODAttributeTypeUniqueID) as? [String]
            guard ids?.first == String(getuid()) else { return false }
            try password.withString { try record.verifyPassword($0) }
            return true
        } catch {
            return false
        }
    }

    // The short name or the full name, as the macOS login window accepts them.
    static func names(_ username: String, short: String = NSUserName(), full: String = NSFullUserName()) -> Bool {
        let matches = { (name: String) in !name.isEmpty && username.compare(name, options: [.caseInsensitive]) == .orderedSame }
        return matches(short) || matches(full)
    }
}

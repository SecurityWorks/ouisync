/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

import Foundation
import MessagePack
import System

//--------------------------------------------------------------------

public class OuisyncRequest {
    let functionName: String
    let functionArguments: MessagePackValue

    init(_ functionName: String, _ functionArguments: MessagePackValue) {
        self.functionName = functionName
        self.functionArguments = functionArguments
    }

    public static func listRepositories() -> OuisyncRequest {
        return OuisyncRequest("list_repositories", MessagePackValue.nil)
    }

    public static func subscribeToRepositoryListChange() -> OuisyncRequest {
        return OuisyncRequest("list_repositories_subscribe", MessagePackValue.nil)
    }

    public static func subscribeToRepositoryChange(_ handle: RepositoryHandle) -> OuisyncRequest {
        return OuisyncRequest("repository_subscribe", MessagePackValue(handle))
    }

    public static func getRepositoryName(_ handle: RepositoryHandle) -> OuisyncRequest {
        return OuisyncRequest("repository_name", MessagePackValue(handle))
    }

    public static func repositoryMoveEntry(_ repoHandle: RepositoryHandle, _ srcPath: FilePath, _ dstPath: FilePath) -> OuisyncRequest {
        return OuisyncRequest("repository_move_entry", MessagePackValue([
            MessagePackValue("repository"): MessagePackValue(repoHandle),
            MessagePackValue("src"): MessagePackValue(srcPath.description),
            MessagePackValue("dst"): MessagePackValue(dstPath.description),
        ]))
    }

    public static func listEntries(_ handle: RepositoryHandle, _ path: FilePath) -> OuisyncRequest {
        return OuisyncRequest("directory_open", MessagePackValue([
            MessagePackValue("repository"): MessagePackValue(handle),
            MessagePackValue("path"): MessagePackValue(path.description),
        ]))
    }

    public static func getEntryVersionHash(_ handle: RepositoryHandle, _ path: FilePath) -> OuisyncRequest {
        return OuisyncRequest("repository_entry_version_hash", MessagePackValue([
          MessagePackValue("repository"): MessagePackValue(handle),
          MessagePackValue("path"): MessagePackValue(path.description),
        ]))
    }

    public static func directoryExists(_ handle: RepositoryHandle, _ path: FilePath) -> OuisyncRequest {
        return OuisyncRequest("directory_exists", MessagePackValue([
            MessagePackValue("repository"): MessagePackValue(handle),
            MessagePackValue("path"): MessagePackValue(path.description),
        ]))
    }

    public static func directoryRemove(_ handle: RepositoryHandle, _ path: FilePath, _ recursive: Bool) -> OuisyncRequest {
        return OuisyncRequest("directory_remove", MessagePackValue([
            MessagePackValue("repository"): MessagePackValue(handle),
            MessagePackValue("path"): MessagePackValue(path.description),
            MessagePackValue("recursive"): MessagePackValue(recursive),
        ]))
    }

    public static func directoryCreate(_ repoHandle: RepositoryHandle, _ path: FilePath) -> OuisyncRequest {
        return OuisyncRequest("directory_create", MessagePackValue([
            MessagePackValue("repository"): MessagePackValue(repoHandle),
            MessagePackValue("path"): MessagePackValue(path.description),
        ]))
    }

    public static func fileOpen(_ repoHandle: RepositoryHandle, _ path: FilePath) -> OuisyncRequest {
        return OuisyncRequest("file_open", MessagePackValue([
            MessagePackValue("repository"): MessagePackValue(repoHandle),
            MessagePackValue("path"): MessagePackValue(path.description),
        ]))
    }

    public static func fileExists(_ handle: RepositoryHandle, _ path: FilePath) -> OuisyncRequest {
        return OuisyncRequest("file_exists", MessagePackValue([
            MessagePackValue("repository"): MessagePackValue(handle),
            MessagePackValue("path"): MessagePackValue(path.description),
        ]))
    }

    public static func fileRemove(_ handle: RepositoryHandle, _ path: FilePath) -> OuisyncRequest {
        return OuisyncRequest("file_remove", MessagePackValue([
            MessagePackValue("repository"): MessagePackValue(handle),
            MessagePackValue("path"): MessagePackValue(path.description),
        ]))
    }

    public static func fileClose(_ fileHandle: FileHandle) -> OuisyncRequest {
        return OuisyncRequest("file_close", MessagePackValue(fileHandle))
    }

    public static func fileRead(_ fileHandle: FileHandle, _ offset: UInt64, _ len: UInt64) -> OuisyncRequest {
        return OuisyncRequest("file_read", MessagePackValue([
            MessagePackValue("file"): MessagePackValue(fileHandle),
            MessagePackValue("offset"): MessagePackValue(offset),
            MessagePackValue("len"): MessagePackValue(len),
        ]))
    }

    public static func fileTruncate(_ fileHandle: FileHandle, _ len: UInt64) -> OuisyncRequest {
        return OuisyncRequest("file_truncate", MessagePackValue([
            MessagePackValue("file"): MessagePackValue(fileHandle),
            MessagePackValue("len"): MessagePackValue(len),
        ]))
    }

    public static func fileLen(_ fileHandle: FileHandle) -> OuisyncRequest {
        return OuisyncRequest("file_len", MessagePackValue(fileHandle))
    }

    public static func fileCreate(_ repoHandle: RepositoryHandle, _ path: FilePath) -> OuisyncRequest {
        return OuisyncRequest("file_create", MessagePackValue([
            MessagePackValue("repository"): MessagePackValue(repoHandle),
            MessagePackValue("path"): MessagePackValue(path.description),
        ]))
    }

    public static func fileWrite(_ fileHandle: FileHandle, _ offset: UInt64, _ data: Data) -> OuisyncRequest {
        return OuisyncRequest("file_write", MessagePackValue([
            MessagePackValue("file"): MessagePackValue(fileHandle),
            MessagePackValue("offset"): MessagePackValue(offset),
            MessagePackValue("data"): MessagePackValue(data),
        ]))
    }
}

//--------------------------------------------------------------------

public class OuisyncRequestMessage {
    public let messageId: MessageId
    public let request: OuisyncRequest

    init(_ messageId: MessageId, _ request: OuisyncRequest) {
        self.messageId = messageId
        self.request = request
    }

    public func serialize() -> [UInt8] {
        var message: [UInt8] = []
        message.append(contentsOf: withUnsafeBytes(of: messageId.bigEndian, Array.init))
        let payload = [MessagePackValue.string(request.functionName): request.functionArguments]
        message.append(contentsOf: pack(MessagePackValue.map(payload)))
        return message
    }

    public static func deserialize(_ data: [UInt8]) -> OuisyncRequestMessage? {
        guard let (id, data) = readMessageId(data) else {
            return nil
        }

        let unpacked = (try? unpack(data))?.0

        guard case let .map(m) = unpacked else { return nil }
        if m.count != 1 { return nil }
        guard let e = m.first else { return nil }
        guard let functionName = e.key.stringValue else { return nil }
        let functionArguments = e.value

        return OuisyncRequestMessage(id, OuisyncRequest(functionName, functionArguments))
    }
}

public class OuisyncResponseMessage {
    public let messageId: MessageId
    public let payload: OuisyncResponsePayload

    public init(_ messageId: MessageId, _ payload: OuisyncResponsePayload) {
        self.messageId = messageId
        self.payload = payload
    }

    public func serialize() -> [UInt8] {
        var message: [UInt8] = []
        message.append(contentsOf: withUnsafeBytes(of: messageId.bigEndian, Array.init))
        let body: MessagePackValue;
        switch payload {
        case .response(let response):
            body = MessagePackValue.map(["success": Self.responseValue(response.value)])
        case .notification(let notification):
            body = MessagePackValue.map(["notification": notification.value])
        case .error(let error):
            let code = Int64(exactly: error.code.rawValue)!
            body = MessagePackValue.map(["failure": .array([.int(code), .string(error.message)])])
        }
        message.append(contentsOf: pack(body))
        return message
    }

    static func responseValue(_ value: MessagePackValue) -> MessagePackValue {
        switch value {
        case .nil: return .string("none")
        default:
            // The flutter code doesn't read the key which is supposed to be a type,
            // would still be nice to have a proper mapping.
            return .map(["todo-type": value])
        }
    }

    public static func deserialize(_ bytes: [UInt8]) -> OuisyncResponseMessage? {
        guard let (id, data) = readMessageId(bytes) else {
            return nil
        }

        let unpacked = (try? unpack(Data(data)))?.0

        if case let .map(m) = unpacked {
            if let success = m[.string("success")] {
                if let value = parseResponse(success) {
                    return OuisyncResponseMessage(id, OuisyncResponsePayload.response(value))
                }
            } else if let error = m[.string("failure")] {
                if let response = parseFailure(error) {
                    return OuisyncResponseMessage(id, OuisyncResponsePayload.error(response))
                }
            } else if let notification = m[.string("notification")] {
                if let value = parseNotification(notification) {
                    return OuisyncResponseMessage(id, OuisyncResponsePayload.notification(value))
                }
            }
        }

        return nil
    }
}

extension OuisyncResponseMessage: CustomStringConvertible {
    public var description: String {
        return "IncomingMessage(\(messageId), \(payload))"
    }
}

fileprivate func readMessageId(_ data: [UInt8]) -> (MessageId, Data)? {
    let idByteCount = (MessageId.bitWidth / UInt8.bitWidth)

    if data.count < idByteCount {
        return nil
    }

    let bigEndianValue = data.withUnsafeBufferPointer {
        ($0.baseAddress!.withMemoryRebound(to: MessageId.self, capacity: 1) { $0 })
    }.pointee

    let id = MessageId(bigEndian: bigEndianValue)

    return (id, Data(data[idByteCount...]))
}
//--------------------------------------------------------------------

public enum OuisyncResponsePayload {
    case response(Response)
    case notification(OuisyncNotification)
    case error(OuisyncError)
}

extension OuisyncResponsePayload: CustomStringConvertible {
    public var description: String {
        switch self {
        case .response(let response):
            return "response(\(response))"
        case .notification(let notification):
            return "notification(\(notification))"
         case .error(let error):
            return "error(\(error))"
        }
    }
}

//--------------------------------------------------------------------

public enum IncomingSuccessPayload {
    case response(Response)
    case notification(OuisyncNotification)
}

extension IncomingSuccessPayload: CustomStringConvertible {
    public var description: String {
        switch self {
        case .response(let value):
            return "response(\(value))"
        case .notification(let value):
            return "notificateion(\(value))"
        }
    }
}

//--------------------------------------------------------------------

public class Response {
    public let value: MessagePackValue

    // Note about unwraps in these methods. It is expected that the
    // caller knows what type the response is. If the expected and
    // the actual types differ, then it is likely that there is a
    // mismatch between the front end and the backend in the FFI API.

    public init(_ value: MessagePackValue) {
        self.value = value
    }

    public func toData() -> Data {
        return value.dataValue!
    }

    public func toUInt64Array() -> [UInt64] {
        return value.arrayValue!.map({ $0.uint64Value! })
    }

    public func toUInt64() -> UInt64 {
        return value.uint64Value!
    }

    public func toBool() -> Bool {
        return value.boolValue!
    }
}

extension Response: CustomStringConvertible {
    public var description: String {
        return "Response(\(value))"
    }
}

//--------------------------------------------------------------------

public class OuisyncNotification {
    let value: MessagePackValue
    init(_ value: MessagePackValue) {
        self.value = value
    }
}

extension OuisyncNotification: CustomStringConvertible {
    public var description: String {
        return "Notification(\(value))"
    }
}

//--------------------------------------------------------------------

func parseResponse(_ value: MessagePackValue) -> Response? {
    if case let .map(m) = value {
        if m.count != 1 {
            return nil
        }
        return Response(m.first!.value)
    } else if case let .string(str) = value, str == "none" {
        // A function was called which has a `void` return value.
        return Response(.nil)
    }
    return nil
}

func parseFailure(_ value: MessagePackValue) -> OuisyncError? {
    if case let .array(arr) = value {
        if arr.count != 2 {
            return nil
        }
        if case let .uint(code) = arr[0] {
            if case let .string(message) = arr[1] {
                guard let codeU16 = UInt16(exactly: code) else {
                    fatalError("Error code from backend is out of range")
                }
                guard let codeEnum = OuisyncErrorCode(rawValue: codeU16) else {
                    fatalError("Invalid error code from backend")
                }
                return OuisyncError(codeEnum, message)
            }
        }
    }
    return nil
}

func parseNotification(_ value: MessagePackValue) -> OuisyncNotification? {
    if case .string(_) = value {
        return OuisyncNotification(MessagePackValue.nil)
    }
    if case let .map(m) = value {
        if m.count != 1 {
            return nil
        }
        return OuisyncNotification(m.first!.value)
    }
    return nil
}

// vz-launch: boot one FreeBSD arm64 guest under Apple's Virtualization.framework
// with nested virtualisation on, so the guest kernel runs with EL2 and can load
// vmm.ko and run bhyve. Solum P0 only (docs/spec/solum.md §10): a functional
// check of the topology, never a source of performance numbers.
//
// Usage: vz-launch --disk DISK.raw [--seed SEED.iso] --nvram NVRAM.fd
//                  [--cpus N] [--mem-gib N] [--mac MAC] [--console LOG]
//
// The guest's virtio console goes to LOG (default: stdout). SIGINT/SIGTERM
// asks the guest to stop, and forces a stop after 60 s.

import Foundation
import Virtualization

func die(_ msg: String) -> Never {
    FileHandle.standardError.write(("vz-launch: " + msg + "\n").data(using: .utf8)!)
    exit(1)
}

var disk: String?
var seed: String?
var nvram: String?
var cpus = 4
var memGiB: UInt64 = 8
var mac = "52:54:00:50:4c:30"
var consolePath: String?

var argv = CommandLine.arguments.dropFirst()
while let a = argv.popFirst() {
    guard let v = argv.popFirst() else { die("missing value for \(a)") }
    switch a {
    case "--disk": disk = v
    case "--seed": seed = v
    case "--nvram": nvram = v
    case "--cpus": guard let n = Int(v), n >= 1, n <= 8 else { die("--cpus 1..8") }; cpus = n
    case "--mem-gib": guard let n = UInt64(v), n >= 2, n <= 16 else { die("--mem-gib 2..16") }; memGiB = n
    case "--mac": mac = v
    case "--console": consolePath = v
    default: die("unknown option \(a)")
    }
}
guard let disk = disk, let nvram = nvram else { die("--disk and --nvram are required") }

// Nested virtualisation is the whole point: refuse rather than boot a guest
// without EL2 and have bhyve fail later with a vaguer error.
guard VZGenericPlatformConfiguration.isNestedVirtualizationSupported else {
    die("this Mac does not offer nested virtualisation (needs M3 or later, macOS 15+)")
}

let config = VZVirtualMachineConfiguration()
let platform = VZGenericPlatformConfiguration()
platform.isNestedVirtualizationEnabled = true
config.platform = platform

let boot = VZEFIBootLoader()
let nvramURL = URL(fileURLWithPath: nvram)
do {
    if FileManager.default.fileExists(atPath: nvram) {
        boot.variableStore = VZEFIVariableStore(url: nvramURL)
    } else {
        boot.variableStore = try VZEFIVariableStore(creatingVariableStoreAt: nvramURL)
    }
} catch { die("EFI variable store: \(error)") }
config.bootLoader = boot

config.cpuCount = cpus
config.memorySize = memGiB * 1024 * 1024 * 1024

var storage: [VZStorageDeviceConfiguration] = []
do {
    let a = try VZDiskImageStorageDeviceAttachment(url: URL(fileURLWithPath: disk), readOnly: false)
    storage.append(VZVirtioBlockDeviceConfiguration(attachment: a))
    if let seed = seed {
        let s = try VZDiskImageStorageDeviceAttachment(url: URL(fileURLWithPath: seed), readOnly: true)
        storage.append(VZVirtioBlockDeviceConfiguration(attachment: s))
    }
} catch { die("disk: \(error)") }
config.storageDevices = storage

let net = VZVirtioNetworkDeviceConfiguration()
net.attachment = VZNATNetworkDeviceAttachment()
guard let macAddr = VZMACAddress(string: mac) else { die("bad --mac \(mac)") }
net.macAddress = macAddr
config.networkDevices = [net]

let serial = VZVirtioConsoleDeviceSerialPortConfiguration()
let out: FileHandle
if let p = consolePath {
    guard FileManager.default.createFile(atPath: p, contents: nil),
          let h = FileHandle(forWritingAtPath: p) else { die("cannot open console log \(p)") }
    out = h
} else {
    out = FileHandle.standardOutput
}
serial.attachment = VZFileHandleSerialPortAttachment(
    fileHandleForReading: FileHandle.standardInput, fileHandleForWriting: out)
config.serialPorts = [serial]
config.entropyDevices = [VZVirtioEntropyDeviceConfiguration()]

do { try config.validate() } catch { die("configuration: \(error)") }

final class Watcher: NSObject, VZVirtualMachineDelegate {
    func guestDidStop(_ vm: VZVirtualMachine) {
        print("vz-launch: guest stopped")
        exit(0)
    }
    func virtualMachine(_ vm: VZVirtualMachine, didStopWithError error: Error) {
        die("guest stopped with error: \(error)")
    }
}

var signalSources: [DispatchSourceSignal] = []
let vm = VZVirtualMachine(configuration: config)
let watcher = Watcher()
vm.delegate = watcher

func requestStop() {
    if vm.canRequestStop {
        try? vm.requestStop()
    }
    DispatchQueue.main.asyncAfter(deadline: .now() + 60) {
        vm.stop { _ in exit(2) }
    }
}
for sig in [SIGINT, SIGTERM] {
    signal(sig, SIG_IGN)
    let src = DispatchSource.makeSignalSource(signal: sig, queue: .main)
    src.setEventHandler { requestStop() }
    src.resume()
    signalSources.append(src)
}

vm.start { result in
    switch result {
    case .success: print("vz-launch: started (\(cpus) vCPUs, \(memGiB) GiB, nested virtualisation on)")
    case .failure(let e): die("start: \(e)")
    }
}
RunLoop.main.run()

// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "stop",
    products: [
        .executable(name: "stop", targets: ["stop"])
    ],
    targets: [
        .executableTarget(
            name: "stop",
            dependencies: []
        )
    ]
)

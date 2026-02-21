// swift-tools-version: 5.9
import PackageDescription

let package = Package(
    name: "rtop",
    products: [
        .executable(name: "rtop", targets: ["rtop"])
    ],
    targets: [
        .executableTarget(
            name: "rtop",
            dependencies: []
        )
    ]
)

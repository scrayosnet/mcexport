![The official Logo of mcexport](.github/images/logo.png "mcexport")

![A visual badge for the latest release](https://img.shields.io/github/v/release/scrayosnet/mcexport "Latest Release")
![A visual badge for the workflow status](https://img.shields.io/github/actions/workflow/status/scrayosnet/mcexport/docker.yml "Workflow Status")
![A visual badge for the dependency status](https://img.shields.io/librariesio/github/scrayosnet/mcexport "Dependencies")
![A visual badge for the Docker image size](https://ghcr-badge.egpl.dev/scrayosnet/mcexport/size "Image Size")
![A visual badge for the license](https://img.shields.io/github/license/scrayosnet/mcexport "License")

mcexport is a [Prometheus][prometheus-docs] prober exporter to query the publicly available information on arbitrary
Minecraft servers through the official [ping protocol][ping-protocol-docs]. Unlike many other available exporters, this
exporter is explicitly designed to query Minecraft servers from the outside.

The idea is similar to the official [Blackbox exporter][blackbox-exporter], but mcexport is specialized in probing
Minecraft servers along with their publicly available Server List information. It supports the Minecraft protocol for
all servers that are using Minecraft version 1.7 and above.

## Motivation

While there are many Minecraft-related exporters available at GitHub, there's none that use the
[Multi-Target Exporter Pattern][multi-target-exporter-docs] and that can be used without exposing [RCON][rcon-docs].
Instead of exposing the internal processes and metrics of a single Minecraft server, mcexport probes any requested
Minecraft server through the publicly available data.

Additionally, mcexport was developed in [Rust][rust-docs], enabling great performance and therefore allowing to scrape
server metrics on a large scale. Combined with its well-tested reliability and hardened security, this makes large-scale
analysis of the Minecraft server ecosystem possible. Therefore, mcexport can be seen as a specialization of the official
[Blackbox exporter][blackbox-exporter] that offers fast and efficient Minecraft metric probing.

The difference between mcexport and other existing solutions like
[sladkoff/minecraft-prometheus-exporter][sladkoff-exporter], [cpburnz/minecraft-prometheus-exporter][cpburnz-exporter]
and [dirien/minecraft-prometheus-exporter][dirien-exporter] is that those specialize in aggregating detailed metrics on
a single Minecraft server (either as a plugin, mod or external service) while mcexport does aggregate only public data
about dynamically submitted Minecraft servers.

The only existing solution that also supports dynamic probing is
[Scientistguy/minecraft-prometheus-exporter][scientist-exporter], which requires [RCON][rcon-docs] to be enabled. That
was not an option, as we want to scrape metrics from servers that we have no control over. Therefore, mcexport is the
only solution for this scenario, offering dynamic probing on public Minecraft servers without any configuration or
necessary adjustments.

Since metrics are bound to a specific instant in time, mcexport needed to be lightweight, scalable and hardened to be
used as a reliable exporter in aggregating data. We offer robust container images and wanted to minimize any
configuration and flags as much as possible. Any configuration is only done within Prometheus, making mcexport
ready for [Kubernetes][kubernetes-docs] from the get-go through [Probe][probe-docs].

## Feature Highlights

* Scrape multiple Minecraft servers through the public [Server List Ping protocol][ping-protocol-docs].
* Expose information about the supported versions, current and maximum player count, latency and player samples.
* Configure the targets and scrape intervals directly within Prometheus.
* Start with zero configuration, no external resources or dependencies for mcexport.
* Use it without applying any changes to the queried Minecraft servers.

## Available Modules

To select a specific protocol version to ping a target, you can use the `module` field of the [Probe Spec][probe-docs]
and set it to the required [protocol version number][pvn-docs]. Any number (negative and positive) is supported and
will be sent as the desired protocol version to the target. mcexport supports the latest iteration of the
[Server List Ping protocol][ping-protocol-docs] that all servers since Minecraft 1.7 use.

If no module is specified explicitly, the most recent version at the time of the last release is used instead (if not
explicitly configured). Therefore, this may not be the latest version at any time, but at least a somewhat recent
version. You can override the version with the `module` field until a new release is created.

Ideally, we could use (and fall back to) `-1` as the protocol version, as that is recommended to use, when determining
the appropriate/maximum supported version of a server. However, our practical tests revealed that only very few servers
would support this convention and would just reply with `-1` on their side, meaning unsupported. And servers that
support `-1` also support specifying a recent version. Therefore, the "somewhat recent" version is the best we can do by
default and without further configuration.

## Getting Started

> [!WARNING]
> While mcexport is stable, please view the individual releases for any version-specific details that need to be
> considered while deploying. Changes are performed in adherence to [Semantic Versioning][semver-docs].

### Setup mcexport

Before any Minecraft servers can be probed, we first need to set up mcexport on the corresponding machine. This does not
have to be the machine of the probed targets, but instead, it will probably be the same system that Prometheus runs on.

#### From Binaries

To run mcexport from a binary file, download the appropriate binary from our [releases][github-releases], make it
executable and run it within the shell of your choice:

```shell
chmod +x mcexport
./mcexport
```

#### Using Docker

To run mcexport within Docker, we can use the images that we release within our [Container Registry][github-ghcr].
Those images are hardened and provide the optimal environment to execute mcexport in a containerized environment.

```shell
docker run --rm \
  -p 10026/tcp \
  --name mcexport \
  ghcr.io/scrayosnet/mcexport:latest
```

#### Using Kubernetes

To deploy mcexport on Kubernetes using Helm, you can use our official Helm chart from GitHub Container Registry:

```shell
helm install mcexport oci://ghcr.io/scrayosnet/helm/mcexport --version 1.0.0
```

You can customize the deployment by providing your own values:

```shell
helm install mcexport oci://ghcr.io/scrayosnet/helm/mcexport --version 1.0.0 -f values.yaml
```

Alternatively, you can create your own deployment using [Kustomize][kustomize-docs] or any other tooling of your choice.

### Check Setup

To verify whether everything works as expected, we can invoke the following command on the same machine and observe the
reported result:

```shell
curl --request GET -sL --url 'http://localhost:10026/probe?target=mc.justchunks.net'
```

If the result shows any metrics, mcexport is now setup successfully and can be used to probe Minecraft servers.

### Configure Prometheus

Now that mcexport is working as expected, we need to configure Prometheus to probe any Minecraft server targets.
Depending on the individual setup, this can be done in one of those ways:

#### Normal Configuration

In a normal (non-Kubernetes) deployment of Prometheus, we can probe any Minecraft server with a scrape
configuration like this:

```yaml
scrape_configs:
- job_name: "mcexport"
  scrape_interval: 60s
  scrape_timeout: 30s
  static_configs:
  - targets:
    - 'mc.justchunks.net'
    - 'example.com'
```

#### CRD-based Configuration

Assuming, there's a namespace `mcexport` with a deployment of mcexport, and a corresponding service `mcexport` is in
that namespace, that exposes the mcexport instances internally, we can probe metrics with this CRD configuration
using the [Probe][probe-docs] resource:

```yaml
apiVersion: monitoring.coreos.com/v1
kind: Probe
metadata:
  name: mcexport
  namespace: mcexport
spec:
  prober:
    url: 'mcexport.mcexport.cluster.local:10026'
  interval: 60s
  scrapeTimeout: 30s
  targets:
    staticConfig:
      static:
      - 'mc.justchunks.net'
      - 'example.com'
```

Depending on your setup, you may also need to add a label, so that the configuration is picked up by your Prometheus
instance. If you've installed it through the `kube-prometheus-stack` helm chart, it could, for example, be
`release: kube-prometheus-stack`. You can check the required labels in your Prometheus CRD.

### Configuration Tweaking

To modify the behavior and global values of mcexport, CLI flags and environment variables may be used. To get an
overview of the available settings and their respective keys, run the following command:

```shell
mcexport --help
```

All settings have sensible defaults that are suitable for production deployments of mcexport. Still, tweaking those
settings can be beneficial for debugging or specific runtime models.

## Reporting Security Issues

To report a security issue for this project, please note our [Security Policy][security-policy].

## Code of Conduct

Participation in this project comes under the [Contributor Covenant Code of Conduct][code-of-conduct].

## How to contribute

Thanks for considering contributing to this project! In order to submit a Pull Request, please read
our [contributing][contributing-guide] guide. This project is in active development, and we're always happy to receive
new contributions!

## License

This project is developed and distributed under the MIT License. See [this explanation][mit-license-doc] for a rundown
on what that means.

[prometheus-docs]: https://prometheus.io/

[ping-protocol-docs]: https://wiki.vg/Server_List_Ping

[blackbox-exporter]: https://github.com/prometheus/blackbox_exporter

[multi-target-exporter-docs]: https://prometheus.io/docs/guides/multi-target-exporter

[rcon-docs]: https://wiki.vg/RCON

[rust-docs]: https://www.rust-lang.org/

[sladkoff-exporter]: https://github.com/sladkoff/minecraft-prometheus-exporter

[cpburnz-exporter]: https://github.com/cpburnz/minecraft-prometheus-exporter

[dirien-exporter]: https://github.com/dirien/minecraft-prometheus-exporter

[scientist-exporter]: https://github.com/Sciencentistguy/minecraft-prometheus-exporter

[kubernetes-docs]: https://kubernetes.io/

[pvn-docs]: https://wiki.vg/Protocol_version_numbers

[probe-docs]: https://github.com/prometheus-operator/prometheus-operator/blob/main/Documentation/api.md#monitoring.coreos.com/v1.Probe

[semver-docs]: https://semver.org/lang/de/

[github-releases]: https://github.com/scrayosnet/mcexport/releases

[github-ghcr]: https://github.com/scrayosnet/mcexport/pkgs/container/mcexport

[helm-chart-docs]: https://helm.sh/

[kustomize-docs]: https://kustomize.io/

[security-policy]: SECURITY.md

[code-of-conduct]: CODE_OF_CONDUCT.md

[contributing-guide]: CONTRIBUTING.md

[mit-license-doc]: https://choosealicense.com/licenses/mit/

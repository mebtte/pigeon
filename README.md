<img src="docs/pigeon.svg" alt="Pigeon logo" width="72" height="72">

# Pigeon

Pigeon is an extremely simple file sharing service for self-hosting.

![](./docs/screenshot.png)

## Deployment

Download Pigeon from [releases](https://github.com/mebtte/pigeon/releases) and start the server.

```sh
pigeon serve --data path_to/data --user 'user1:password1' --user 'user2:password2'
```

Then you can view it on "http://localhost:3000". Also you can use docker.

### Docker CLI

```sh
docker run -d --name pigeon \
  -p 3000:3000 \
  -v pigeon-data:/data \
  mebtte/pigeon:v0 \
  serve --data /data --user 'user1:password1'
```

### Docker Compose

```yaml
services:
  pigeon:
    restart: always
    container_name: pigeon
    image: mebtte/pigeon:v0
    ports:
      - 3000:3000
    volumes:
      - path_to/data:/data
    command: ["serve", "--data", "/data", "--user", "user1:password1"]
```

## Usage

```console
$ pigeon -h
Pigeon is an extremely simple file sharing service for self-hosting.

Usage: pigeon <COMMAND>

Commands:
  serve  Start the Pigeon server
  share  Upload files or directories and create a share
  help   Print this message or the help of the given subcommand(s)

Options:
  -h, --help     Print help
  -V, --version  Print version
```

```console
$ pigeon serve -h
Start the Pigeon server

Usage: pigeon serve [OPTIONS]

Options:
      --user <USERNAME:PASSWORD>  Login account; repeat to add users
      --port <PORT>               HTTP port [default: 3000]
      --base-path <BASE_PATH>     Public deployment path [default: /]
      --max-file-size <SIZE>      Maximum upload file size, e.g. 500MB or 2GB. Omit for unlimited.
      --data <DATA>               Persistent files and SQLite database [default: ./pigeon-data]
  -h, --help                      Print help
```

```console
$ pigeon share -h
Upload files or directories and create a share

Usage: pigeon share --endpoint <URL> --user <USERNAME:PASSWORD> <INPUT>...

Arguments:
  <INPUT>...  Files or directories to share

Options:
      --endpoint <URL>            Pigeon deployment URL
      --user <USERNAME:PASSWORD>  Pigeon login account
  -h, --help                      Print help
```

## License

Pigeon is licensed under the [GNU General Public License v3.0](LICENSE) (`GPL-3.0-only`).

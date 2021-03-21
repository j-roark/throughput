# UDP Throughput Testing

This program is designed to test the UDP throughput of a connection. It tests the speed at which a packet takes to send from the host to the client, it then tests how long it takes to send from the client back to the host. This can effectively test differences in directional communication speeds. This is useful as other methods test the round trip timings but this method tests monodirectional timings.

## Installation
```
cargo build
```

## Usage
```
throughput -c (client true/false) -d (destination) -s (todo: speed) -z (size) -p (port) -t (time(s))
```

## Contributing
Pull requests are welcome. For major changes, please open an issue first to discuss what you would like to change.

Please make sure to update tests as appropriate.

## License
[MIT](https://choosealicense.com/licenses/mit/)

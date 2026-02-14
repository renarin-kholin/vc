
# Voice Chat
For now, this isn't anything impressive. It just uses your default input and output devices, creates streams for the respectively and then sends opus packets over the QUIC transport on a Stream protocol. This is just a proof of concept.

## Usage
### Running the relay server
```console
renarin@wandersail:~$ cargo run --bin server -- --port $PORT --secret-key-seed $SECRET
```
### Running the client 
```console
renarin@wandersail:~$ cargo run --bin voice_chat
Enter relay address or enter to use without relay: # Enter a relay address from the server output
Enter an address to dial or press enter to wait for connection: # Press enter and in another terminal run the client again and paste the multiaddress that shows up in the end
```



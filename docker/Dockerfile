FROM rust:1.92-trixie
RUN useradd -d /code -c "The User" -M user
RUN apt update 
RUN apt install -y libudev-dev
# USER user
# RUN cd ~ &&  cargo install cargo-deb

# cargo install cargo-deb
# cargo deb
# rustup target add x86_64-pc-windows-gnu
# apt install mingw-w64
# cargo build --target x86_64-pc-windows-gnu

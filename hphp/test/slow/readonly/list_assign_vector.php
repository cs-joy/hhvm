<?hh
<<file:__EnableUnstableFeatures('readonly')>>
class Baz {
}
class Foo {
  public int $prop;
  public readonly vec<Foo> $bar = vec[];
  public readonly Baz $baz;
  public function __construct() {
    $this->prop = 1;
    $this->baz = new Baz();
  }

}
<<__EntryPoint>>
function main(): void{
  $y = readonly Vector{ new Foo(), new Foo() };
  $z = readonly Vector { new Foo() };
  list($z[0], $z[1]) = $y;
}
